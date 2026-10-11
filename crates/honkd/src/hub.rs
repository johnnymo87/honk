use anyhow::Context;
use honk_upstream::{Recorder, Redactor, SubscribeOpts, Upstream, UpstreamHandle, UpstreamOpts};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::Args;
use crate::state::StateStore;

pub struct Hub {
    pub args: Arc<Args>,
    pub state: Arc<Mutex<StateStore>>,
    pub upstream_url: String,
    pub upstream_secret: String,
    pub control: UpstreamHandle,
    pub control_init_result: Value,
}

impl Hub {
    pub async fn start(
        args: Arc<Args>,
        state_store: StateStore,
        upstream_secret: String,
    ) -> anyhow::Result<Arc<Self>> {
        let state = Arc::new(Mutex::new(state_store));
        let upstream_url = args.upstream.clone();

        // 1. Collect orphan ids and old control connection from state
        let (orphans, old_control) = {
            let s = state.lock().await;
            (s.orphan_conns.clone(), s.control_conn.clone())
        };

        // 2. DELETE orphan ids and old control connection, best effort
        let upstream_opts = UpstreamOpts {
            allow_production: args.allow_production_upstream,
            ..Default::default()
        };

        let mut to_delete = orphans;
        if let Some(ctrl) = old_control {
            to_delete.push(ctrl);
        }

        for cid in to_delete {
            if let Ok(up) = Upstream::resume_with(
                &upstream_url,
                upstream_secret.clone(),
                cid,
                Recorder::disabled(),
                &upstream_opts,
            ) {
                let _ = up.delete().await;
            }
        }

        // Clear orphan conns and old control connection in state
        {
            let mut s = state.lock().await;
            s.mutate(|st| {
                st.orphan_conns.clear();
                st.control_conn = None;
            })?;
        }

        // 3. Initialize a new control connection, persisting its id first
        let recorder = if let Some(rec_path) = &args.record {
            let redactor = Redactor::from_env();
            Recorder::file(rec_path, redactor)?
        } else {
            Recorder::disabled()
        };

        let mut control_upstream = Upstream::initialize_with(
            &upstream_url,
            upstream_secret.clone(),
            recorder,
            &upstream_opts,
        )
        .await
        .context("failed to initialize upstream control connection")?;

        let conn_id = control_upstream.conn_id().to_string();

        // Persist its id first BEFORE subscribing!
        {
            let mut s = state.lock().await;
            s.mutate(|st| {
                st.control_conn = Some(conn_id.clone());
            })?;
        }

        // 4. Subscribe its conn stream with SubscribeOpts { takeover: true, resume_after: None }
        let _ = control_upstream
            .subscribe_with(
                None,
                SubscribeOpts {
                    takeover: true,
                    resume_after: None,
                },
            )
            .await
            .context("failed to subscribe control conn stream")?;

        // 5. Read stream_features()
        let features = control_upstream.stream_features();
        let is_patched = features.map(|f| f.is_patched()).unwrap_or(false);
        if !is_patched && !args.allow_unpatched_upstream {
            anyhow::bail!(
                "upstream goose lacks Acp-Stream-Features takeover+replay; run patched-1.54.0.3 or pass --allow-unpatched-upstream"
            );
        }

        let control_init_result = control_upstream
            .init_result()
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));

        // 6. Split control connection
        let (control_handle, mut control_rx) = control_upstream.split();
        tokio::spawn(async move {
            while let Some(_event) = control_rx.recv().await {
                // Control connection conn stream events
            }
        });

        // 7. Register session handles from state (Task A12 fills this in)

        Ok(Arc::new(Self {
            args,
            state,
            upstream_url,
            upstream_secret,
            control: control_handle,
            control_init_result,
        }))
    }

    pub fn handle_initialize(&self, _params: &Value) -> Value {
        let mut result_obj = match self.control_init_result.get("result") {
            Some(res) => res.clone(),
            None => self.control_init_result.clone(),
        };
        if !result_obj.is_object() {
            result_obj = serde_json::json!({});
        }

        if let Some(meta_val) = result_obj.get_mut("_meta").and_then(|v| v.as_object_mut()) {
            meta_val.insert(
                "honk".to_string(),
                serde_json::json!({
                    "protocol": honk_wire::PROTOCOL,
                    "version": env!("CARGO_PKG_VERSION"),
                }),
            );
        } else {
            result_obj["_meta"] = serde_json::json!({
                "honk": {
                    "protocol": honk_wire::PROTOCOL,
                    "version": env!("CARGO_PKG_VERSION"),
                }
            });
        }

        result_obj
    }
}
