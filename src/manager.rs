use crate::{
    Error, MissingObjectKey, PodPatchFailed, Result, SerializationFailed,
    ZooKeeperClusterPatchFailed,
};

use chrono::prelude::*;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use k8s_openapi::api::core::v1::{
    Affinity, Container, Pod, PodAffinityTerm, PodAntiAffinity, PodSpec, Toleration,
};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1beta1::CustomResourceDefinition;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, OwnerReference, Time};
use kube::api::{ObjectMeta, PatchStrategy, PostParams};
use kube::{
    api::{Api, ListParams, Meta, PatchParams},
    client::Client,
    CustomResource,
};
use kube_runtime::controller::{Context, Controller, ReconcilerAction};
use prometheus::{default_registry, proto::MetricFamily, register_int_counter, IntCounter};
use serde::{Deserialize, Serialize};
use serde_json::json;
use snafu::{Backtrace, OptionExt, ResultExt, Snafu};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::{sync::RwLock, time::Duration};
use tracing::{debug, error, info, instrument, trace, warn};

#[derive(Clone, CustomResource, Debug, Deserialize, Serialize)]
#[kube(
    group = "zookeeper.stackable.de",
    version = "v1",
    kind = "ZooKeeperCluster",
    shortname = "zk",
    namespaced
)]
#[kube(status = "ZooKeeperClusterStatus")]
pub struct ZooKeeperClusterSpec {
    version: ZooKeeperVersion,
    replicas: i32,
}

#[allow(non_camel_case_types)]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ZooKeeperVersion {
    #[serde(rename = "3.6.2")]
    v3_6_2,

    #[serde(rename = "3.5.8")]
    v3_5_8,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct ZooKeeperClusterStatus {
    is_bad: bool,
}

// Context for our reconciler
#[derive(Clone)]
struct Data {
    /// kubernetes client
    client: Client,
    /// In memory state
    state: Arc<RwLock<State>>,
    /// Various prometheus metrics
    metrics: Metrics,
}

fn create_tolerations() -> Vec<Toleration> {
    vec![
        Toleration {
            effect: Some(String::from("NoExecute")),
            key: Some(String::from("kubernetes.io/arch")),
            operator: Some(String::from("Equal")),
            toleration_seconds: None,
            value: Some(String::from("stackable-linux")),
        },
        Toleration {
            effect: Some(String::from("NoSchedule")),
            key: Some(String::from("kubernetes.io/arch")),
            operator: Some(String::from("Equal")),
            toleration_seconds: None,
            value: Some(String::from("stackable-linux")),
        },
        Toleration {
            effect: Some(String::from("NoSchedule")),
            key: Some(String::from("node.kubernetes.io/network-unavailable")),
            operator: Some(String::from("Exists")),
            toleration_seconds: None,
            value: None,
        },
    ]
}

const FINALIZER: &str = "zookeeper.stackable.de/check-stuff";
const FIELD_MANAGER: &str = "zookeeper.stackable.de";

fn object_to_owner_reference<K: Meta>(meta: ObjectMeta) -> Result<OwnerReference, Error> {
    Ok(OwnerReference {
        api_version: K::API_VERSION.to_string(),
        kind: K::KIND.to_string(),
        name: meta.name.context(MissingObjectKey {
            name: ".metadata.name",
        })?,
        uid: meta.uid.context(MissingObjectKey {
            name: ".metadata.backtrace",
        })?,
        ..OwnerReference::default()
    })
}

// This method is called for every modification of our object (this includes creation).
// It will _not_ be called for deletions as deletions might be missed when the Operator is offline.
// Therefore to handle deletions a concept called `Finalizers` are used.
// For more information see here: https://kubernetes.io/docs/tasks/extend-kubernetes/custom-resources/custom-resource-definitions/#finalizers
#[instrument(skip(ctx))]
async fn reconcile(
    zk_cluster: ZooKeeperCluster,
    ctx: Context<Data>,
) -> Result<ReconcilerAction, Error> {
    let client = ctx.get_ref().client.clone();

    ctx.get_ref().state.write().await.last_event = Utc::now();
    let name = Meta::name(&zk_cluster);
    let ns = Meta::namespace(&zk_cluster).expect("ZooKeeperCluster is namespaced");
    debug!("Reconcile ZooKeeperCluster [{}]: {:?}", name, zk_cluster);

    let zookeeper_clusters: Api<ZooKeeperCluster> = Api::namespaced(client.clone(), &ns);

    // Handel object deletion
    let ps = PatchParams::default(); //TODO: fix default_apply().force()

    // TODO: zk_clusters shouldn't be cloned, pass reference instead
    if handle_deletion(zk_cluster.clone(), &name, &zookeeper_clusters, &ps).await? {
        // TODO: Clean up pods....

        return Ok(ReconcilerAction {
            requeue_after: None,
        });
    }

    // Here we've already handled deletions so now we're sure that this change is some other change

    let new_status = serde_json::to_vec(&json!({
        "status": ZooKeeperClusterStatus {
            is_bad: false,
        }
    }))
    .context(SerializationFailed)?;

    let _o = zookeeper_clusters
        .patch_status(&name, &ps, new_status)
        .await
        .context(ZooKeeperClusterPatchFailed)?;

    let mut labels = BTreeMap::new();
    labels.insert("zookeeper-name".to_string(), name.clone());

    for i in 0..zk_cluster.spec.replicas {
        let pod_name = format!("{}-{}", name, i);
        let pod = Pod {
            metadata: ObjectMeta {
                name: Some(pod_name.clone()),
                owner_references: Some(vec![OwnerReference {
                    controller: Some(true),
                    ..object_to_owner_reference::<ZooKeeperCluster>(zk_cluster.metadata.clone())?
                }]),
                labels: Some(labels.clone()),
                ..ObjectMeta::default()
            },
            spec: Some(PodSpec {
                tolerations: Some(create_tolerations()),
                containers: vec![Container {
                    image: Some(format!("stackable/zookeeper:{:?}", zk_cluster.spec.version)),
                    name: "zookeeper".to_string(),
                    ..Container::default()
                }],
                affinity: Some(Affinity {
                    pod_anti_affinity: Some(PodAntiAffinity {
                        required_during_scheduling_ignored_during_execution: Some(vec![
                            PodAffinityTerm {
                                label_selector: Some(LabelSelector {
                                    match_labels: Some(labels.clone()),
                                    ..LabelSelector::default()
                                }),
                                topology_key: "kubernetes.io/hostname".to_string(),
                                ..PodAffinityTerm::default()
                            },
                        ]),
                        ..PodAntiAffinity::default()
                    }),
                    ..Affinity::default()
                }),
                ..PodSpec::default()
            }),
            ..Pod::default()
        };

        let pods_api: Api<Pod> = Api::namespaced(client.clone(), &ns);
        pods_api
            .patch(
                &pod_name,
                &PatchParams {
                    patch_strategy: PatchStrategy::Apply,
                    field_manager: Some(FIELD_MANAGER.to_string()),
                    ..PatchParams::default()
                },
                serde_json::to_vec(&pod).context(SerializationFailed)?,
            )
            .await
            .context(PodPatchFailed)?;
    }

    debug!("Done applying!");

    ctx.get_ref().metrics.handled_events.inc();

    // If no events were received, check back every 30 minutes
    Ok(ReconcilerAction {
        requeue_after: Some(Duration::from_secs(3600 / 2)),
    })
}

// If our object has a deletion timestamp it is scheduled to be deleted and it can't be changed
// with the exception of the finalizer list.
async fn handle_deletion(
    zk_cluster: ZooKeeperCluster,
    name: &String,
    zookeeper_clusters: &Api<ZooKeeperCluster>,
    ps: &PatchParams,
) -> Result<bool> {
    return Ok(false);
    if let Some(deletion_timestamp) = zk_cluster.metadata.deletion_timestamp {
        debug!(
            "The object is in the process of being deleted. Deletion timestamp: [{:?}]",
            deletion_timestamp
        );

        // Now we need to check whether the list of finalizers includes our own finalizer.
        if let Some(finalizers) = zk_cluster.metadata.finalizers {
            let mut finalizers: Vec<String> = finalizers;
            let index = finalizers
                .iter()
                .position(|finalizer| finalizer == FINALIZER);
            if let Some(index) = index {
                // We found our finalizer which means that we now need to handle our deletion logic
                // And then remove the finalizer from the list.

                finalizers.swap_remove(index);
                let new_metadata = serde_json::to_vec(&json!({
                    "metadata": {
                        "finalizers": finalizers
                    }
                }))
                .context(SerializationFailed)?;
                let _o = zookeeper_clusters
                    .patch(&name, &ps, new_metadata)
                    .await
                    .context(ZooKeeperClusterPatchFailed)?;

                return Ok(true);
            }
        }
    } else {
        // The object is not deleted but we need to check whether our finalizer is already in the finalizer list
        // If not we'll add it.
        let mut finalizers: Vec<String> = zk_cluster.metadata.finalizers.unwrap_or_default();

        if !finalizers.contains(&FINALIZER.to_string()) {
            finalizers.push(FINALIZER.to_string());

            let new_metadata = serde_json::to_vec(&json!({
                "metadata": {
                    "finalizers": finalizers
                }
            }))
            .context(SerializationFailed)?;
            let _o = zookeeper_clusters
                .patch(&name, &ps, new_metadata)
                .await
                .context(ZooKeeperClusterPatchFailed)?;
        }
    }
    Ok(false)
}

fn error_policy(error: &Error, _ctx: Context<Data>) -> ReconcilerAction {
    warn!("reconcile failed: {}", error);
    ReconcilerAction {
        requeue_after: Some(Duration::from_secs(360)),
    }
}

/// Metrics exposed on /metrics
#[derive(Clone)]
pub struct Metrics {
    pub handled_events: IntCounter,
}

impl Metrics {
    fn new() -> Self {
        Metrics {
            handled_events: register_int_counter!("handled_events", "handled events").unwrap(),
        }
    }
}

/// In-memory reconciler state exposed on /
#[derive(Clone, Serialize)]
pub struct State {
    #[serde(deserialize_with = "from_ts")]
    pub last_event: DateTime<Utc>,
}

impl State {
    fn new() -> Self {
        State {
            last_event: Utc::now(),
        }
    }
}

/// Data owned by the Manager
#[derive(Clone)]
pub struct Manager {
    /// In memory state
    state: Arc<RwLock<State>>,
    /// Various prometheus metrics
    metrics: Metrics,
}

impl Manager {
    /// Lifecycle initialization interface for app
    ///
    /// This returns a `Manager` that drives a `Controller` + a future to be awaited
    /// It is up to `main` to wait for the controller stream.
    pub async fn new(client: Client) -> (Self, BoxFuture<'static, ()>) {
        // Check whether the ZooKeeperClusters CRD has been registered and fail if not
        let crds: Api<CustomResourceDefinition> = Api::all(client.clone());
        crds.get("zookeeperclusters.zookeeper.stackable.de")
            .await
            .expect("ZooKeeperCluster CRD is missing!");

        let metrics = Metrics::new();
        let state = Arc::new(RwLock::new(State::new()));
        let context = Context::new(Data {
            client: client.clone(),
            metrics: metrics.clone(),
            state: state.clone(),
        });

        let zookeeper_clusters_api = Api::<ZooKeeperCluster>::all(client.clone());
        let pods_api = Api::<Pod>::all(client);

        // It does not matter what we do with the stream returned from `run`
        // but we do need to consume it, that's why we return a future.
        let drainer = Controller::new(zookeeper_clusters_api, ListParams::default())
            .owns(pods_api, ListParams::default())
            .run(reconcile, error_policy, context)
            .filter_map(|x| async move { std::result::Result::ok(x) })
            .for_each(|_| futures::future::ready(()))
            .boxed();

        (Self { state, metrics }, drainer)
    }

    /// Metrics getter
    pub fn metrics(&self) -> Vec<MetricFamily> {
        default_registry().gather()
    }

    /// State getter
    pub async fn state(&self) -> State {
        self.state.read().await.clone()
    }
}