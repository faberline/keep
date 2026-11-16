//! keep's operator wiring onto the shared `libs/service-k8s` controller.
//!
//! The reconcile loop + leader-election lease live in `libs/service-k8s`
//! (`service_k8s::run` drives the watch + leader-gated server-side apply over
//! kube-rs). keep supplies only its [`ManagedService`] impl — what to render,
//! which workload to poll for readiness, and the `Keep` status subresource to
//! write.
//!
//! @spec .aw/tech-design/projects/keep/interfaces/cli/adopt-libs-operator-keep-k8s-crd-operator-instance-cli.md

use k8s_openapi::api::apps::v1::StatefulSet;
use kube::{Api, Client, ResourceExt};
use serde_json::{json, Value};
use service_k8s::{ManagedService, ReadinessTarget, ReadyFacts, ReconcilePlan};

use super::crd::Keep;
use super::membership_http::HttpMembershipClient;
use super::render;
use super::scale;

const MEMBERS_ENV: &str = "KEEP_RAFT_MEMBERS_PER_SHARD";

/// Confirmed voters per shard in the live StatefulSet. `None` (no StatefulSet
/// yet, the first apply) gives `wanted`. Otherwise the value of the env
/// `KEEP_RAFT_MEMBERS_PER_SHARD` of the server container when it parses as a
/// `u32 >= 1`, else `replicas / shard_count` (an operator upgrade: the env is
/// not there yet), and `wanted` when that is 0.
///
/// The env keeps the confirmed value when a scale-up call is refused: the
/// StatefulSet then has the new pod count, but the voters are still the old ones.
pub(crate) fn live_members_per_shard(
    live: Option<&StatefulSet>,
    shard_count: u32,
    wanted: u32,
) -> u32 {
    let Some(sts) = live else {
        return wanted;
    };
    let from_env = sts
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .into_iter()
        .flat_map(|pod| pod.containers.iter())
        .flat_map(|container| container.env.iter().flatten())
        .find(|env| env.name == MEMBERS_ENV)
        .and_then(|env| env.value.as_deref())
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|members| *members >= 1);
    if let Some(members) = from_env {
        return members;
    }
    let replicas = sts
        .spec
        .as_ref()
        .and_then(|spec| spec.replicas)
        .unwrap_or(0)
        .max(0) as u32;
    match replicas / shard_count.max(1) {
        0 => wanted,
        members => members,
    }
}

/// `patch` with the pending membership lines appended to `status.message`:
/// `"<message>; membership pending: <line>; <line>"`. An empty or missing
/// `context["pending"]` leaves the patch unchanged.
pub(crate) fn with_pending(mut patch: Value, context: &Value) -> Value {
    let pending: Vec<&str> = context["pending"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if pending.is_empty() {
        return patch;
    }
    let message = patch["status"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    patch["status"]["message"] = json!(format!(
        "{message}; membership pending: {}",
        pending.join("; ")
    ));
    patch
}

impl ManagedService for Keep {
    /// Server-side-apply field manager + leader-election Lease name.
    const MANAGER: &'static str = "keep-operator";

    fn render(&self) -> Vec<serde_json::Value> {
        render::render(self)
    }

    fn readiness_targets(&self) -> Vec<ReadinessTarget> {
        // keep is always a StatefulSet (durable disk tier); poll it for
        // `.status.readyReplicas`.
        vec![ReadinessTarget::new("StatefulSet", self.name_any())]
    }

    fn status_patch(&self, ready: &ReadyFacts) -> serde_json::Value {
        let name = self.name_any();
        let ready_replicas = ready.get(&name) as i32;
        let desired = (self.spec.cluster.shard_count * self.spec.cluster.replicas_per_shard) as i32;
        let phase = if desired > 0 && ready_replicas >= desired {
            "Ready"
        } else if ready_replicas > 0 {
            "Reconciling"
        } else {
            "Pending"
        };
        json!({ "status": {
            "phase": phase,
            "observedGeneration": self.metadata.generation.unwrap_or(0),
            "readyReplicas": ready_replicas,
            "desiredReplicas": desired,
            "shardCount": self.spec.cluster.shard_count,
            "message": format!("{ready_replicas}/{desired} store pods ready"),
        }})
    }

    /// Drive raft membership before the pods change: read the live StatefulSet,
    /// call [`scale::drive`] when the voters differ from the spec, and render the
    /// StatefulSet with the replicas it allows. On the first apply no membership
    /// call is made.
    async fn reconcile_plan(&self, client: Client) -> anyhow::Result<ReconcilePlan> {
        let name = self.name_any();
        let ns = self.namespace().unwrap_or_else(|| "default".into());
        let shard_count = self.spec.cluster.shard_count.max(1);
        let wanted = self.spec.cluster.replicas_per_shard.max(1);
        let live = Api::<StatefulSet>::namespaced(client, &ns)
            .get_opt(&name)
            .await?;
        let members = live_members_per_shard(live.as_ref(), shard_count, wanted);
        let membership = HttpMembershipClient::new(&name, shard_count, members);
        let outcome = scale::drive(&membership, &name, shard_count, members, wanted).await;
        let children = render::render_scaled(
            self,
            outcome.replicas_per_shard_allowed,
            outcome.members_per_shard,
        );
        Ok(ReconcilePlan::new(
            children,
            json!({ "pending": outcome.pending }),
        ))
    }

    fn status_patch_with_context(&self, ready: &ReadyFacts, context: &Value) -> Value {
        with_pending(self.status_patch(ready), context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A live StatefulSet `store` with `replicas` pods and, when given, the
    /// env `KEEP_RAFT_MEMBERS_PER_SHARD` on its server container.
    fn live_sts(replicas: i32, env: Option<&str>) -> StatefulSet {
        let mut container = json!({ "name": "keep", "image": "keep:test" });
        if let Some(value) = env {
            container["env"] = json!([{ "name": MEMBERS_ENV, "value": value }]);
        }
        serde_json::from_value(json!({
            "apiVersion": "apps/v1",
            "kind": "StatefulSet",
            "metadata": { "name": "store" },
            "spec": {
                "replicas": replicas,
                "serviceName": "store-headless",
                "selector": { "matchLabels": { "app": "keep" } },
                "template": {
                    "metadata": { "labels": { "app": "keep" } },
                    "spec": { "containers": [container] },
                },
            },
        }))
        .expect("valid StatefulSet")
    }

    #[test]
    fn live_members_none_gives_wanted() {
        assert_eq!(live_members_per_shard(None, 3, 2), 2);
    }

    #[test]
    fn live_members_read_the_env() {
        // A refused scale-up keeps the old voters even though the pods are there.
        let live = live_sts(6, Some("1"));
        assert_eq!(live_members_per_shard(Some(&live), 3, 2), 1);
    }

    #[test]
    fn live_members_fall_back_to_replicas_over_shards() {
        let live = live_sts(6, None);
        assert_eq!(live_members_per_shard(Some(&live), 3, 2), 2);
    }

    #[test]
    fn live_members_ignore_a_bad_env() {
        for bad in ["0", "x"] {
            let live = live_sts(6, Some(bad));
            assert_eq!(live_members_per_shard(Some(&live), 3, 2), 2, "env {bad}");
        }
    }

    #[test]
    fn with_pending_appends_the_lines() {
        let patch = json!({ "status": { "message": "1/2 store pods ready" } });
        let context = json!({ "pending": ["shard 0 replica 1: not_caught_up"] });
        assert_eq!(
            with_pending(patch, &context)["status"]["message"],
            "1/2 store pods ready; membership pending: shard 0 replica 1: not_caught_up"
        );
    }

    #[test]
    fn with_pending_empty_is_unchanged() {
        let patch = json!({ "status": { "message": "1/2 store pods ready" } });
        assert_eq!(
            with_pending(patch.clone(), &json!({ "pending": [] })),
            patch
        );
        assert_eq!(with_pending(patch.clone(), &Value::Null), patch);
    }
}

/// `keep k8s operator run` — run the reconcile controller on the shared
/// `libs/service-k8s` host (leader-gated; safe at `replicas > 1`).
pub async fn run() -> anyhow::Result<()> {
    service_k8s::run::<Keep>().await
}
