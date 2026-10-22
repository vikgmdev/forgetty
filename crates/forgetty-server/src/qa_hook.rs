//! Test-only `qa_*` socket methods (feature `qa-tools`).
//!
//! The protocol harness in `forgetty-client` drives a real server through the
//! local socket with these methods: counters, fault injection and log access.
//! The feature is off in release builds, so a shipped server does not have
//! them.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use base64::Engine as _;
use forgetty_core::PaneId;
use forgetty_proto::ServerEvent;
use forgetty_session::{LogPlan, SessionManager};
use forgetty_socket::protocol::{Request, Response, INTERNAL_ERROR, INVALID_PARAMS};
use forgetty_sync::registry::{iso8601_now, DeviceEntry};
use forgetty_sync::SyncEndpoint;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::rpc_v2::RpcV2;

/// Most log bytes `qa_read_log` returns.
const READ_CAP: usize = 16 * 1024 * 1024;

/// Register the `qa_*` methods with the socket dispatch table.
pub fn install(rpc: Arc<RpcV2>, sync: Arc<SyncEndpoint>) {
    forgetty_socket::qa::set_hook(Arc::new(move |req, sm| handle(&rpc, &sync, req, sm)));
}

fn bad(req: &Request, msg: &str) -> Response {
    Response::error(req.id.clone(), INVALID_PARAMS, msg.to_string())
}

#[allow(clippy::result_large_err)]
fn pane_param(req: &Request) -> Result<Uuid, Response> {
    req.params
        .get("pane_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| bad(req, "pane_id must be a uuid string"))
}

fn handle(
    rpc: &Arc<RpcV2>,
    sync: &Arc<SyncEndpoint>,
    req: &Request,
    sm: &Arc<SessionManager>,
) -> Option<Response> {
    let ok = |v: Value| Some(Response::success(req.id.clone(), v));
    match req.method.as_str() {
        "qa_state" => {
            let panes: Vec<Value> = sm
                .list_panes()
                .into_iter()
                .filter_map(|id| {
                    let info = sm.pane_info(id)?;
                    let (first, total) = sm.log_bounds(id).unwrap_or((0, 0));
                    Some(json!({
                        "pane_id": id.0.to_string(), "rows": info.rows, "cols": info.cols,
                        "first": first, "total": total,
                    }))
                })
                .collect();
            let input: Vec<Value> = sm
                .input_reports()
                .into_iter()
                .map(|r| {
                    json!({
                        "pane_id": r.pane_id.0.to_string(),
                        "queued_bytes": r.writer.queued_bytes,
                        "frames": r.writer.frames,
                        "in_write_ms": r.writer.in_write.map(|d| d.as_millis() as u64),
                        "since_progress_ms": r.writer.since_progress.as_millis() as u64,
                        "writer_exited": r.writer.exited,
                        "writer_state": r.writer.writer_state(),
                        "stalled": r.stalled(),
                        "reader": r.reader.to_string(),
                        "child_pid": r.child_pid,
                        "child_alive": r.child_alive,
                    })
                })
                .collect();
            let attaches: Vec<Value> = rpc
                .qa
                .attaches
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|(p, o)| json!([p.to_string(), o]))
                .collect();
            let port = sync.bound_sockets().into_iter().find(|s| s.is_ipv4()).map(|s| s.port());
            ok(json!({
                "node_id": sync.node_id().to_string(),
                "udp_port": port,
                "dedup_drops": rpc.qa.dedup_drops.load(Ordering::SeqCst),
                "handshakes": rpc.qa.handshakes.load(Ordering::SeqCst),
                "stopping_refusals": rpc.qa.stopping_refusals.load(Ordering::SeqCst),
                "catchups": rpc.qa.catchups.load(Ordering::SeqCst),
                "last_snapshot_us": rpc.snapshots.stats.last_us.load(Ordering::SeqCst),
                "last_snapshot_bytes": rpc.snapshots.stats.last_bytes.load(Ordering::SeqCst),
                "snapshot_builds": rpc.snapshots.stats.builds.load(Ordering::SeqCst),
                "snapshot_joins": rpc.snapshots.stats.joins.load(Ordering::SeqCst),
                "snapshot_reuses": rpc.snapshots.stats.reuses.load(Ordering::SeqCst),
                "snapshot_cancelled": rpc.snapshots.stats.cancelled.load(Ordering::SeqCst),
                "snapshot_max_running": rpc.snapshots.stats.max_running.load(Ordering::SeqCst),
                "input_drops": rpc.qa.input_drops.load(Ordering::SeqCst),
                "auto_drops": rpc.qa.auto_drops.load(Ordering::SeqCst),
                "too_many_closes": rpc.qa.too_many_closes.load(Ordering::SeqCst),
                "stall_closes": rpc.qa.stall_closes.load(Ordering::SeqCst),
                "alive_tasks": tokio::runtime::Handle::current().metrics().num_alive_tasks(),
                "connections": rpc.connection_count(),
                "quic_open": sync.quic_open(),
                "handshake_failures": sync.handshake_failures(),
                "attaches": attaches,
                "panes": panes,
                "pane_input": input,
            }))
        }
        "qa_drop_connections" => {
            rpc.drop_connections();
            ok(json!({ "ok": true }))
        }
        "qa_pause_handshakes" => {
            let paused = req.params.get("paused").and_then(|v| v.as_bool()).unwrap_or(true);
            rpc.qa.paused.send_replace(paused);
            ok(json!({ "ok": true }))
        }
        "qa_hold_acks" => {
            let hold = req.params.get("hold").and_then(|v| v.as_bool()).unwrap_or(true);
            rpc.qa.hold_acks.store(hold, Ordering::SeqCst);
            ok(json!({ "ok": true }))
        }
        "qa_stopping" => {
            let on = req.params.get("on").and_then(|v| v.as_bool()).unwrap_or(true);
            if on {
                sm.clients().begin_stop();
            } else {
                sm.clients().cancel_stop();
            }
            ok(json!({ "ok": true }))
        }
        "qa_open_access" => {
            let on = req.params.get("on").and_then(|v| v.as_bool()).unwrap_or(true);
            rpc.qa.open_access.store(on, Ordering::SeqCst);
            ok(json!({ "ok": true }))
        }
        "qa_authorize" => {
            let Some(id) = req.params.get("device_id").and_then(|v| v.as_str()) else {
                return Some(bad(req, "device_id is required"));
            };
            let entry = DeviceEntry {
                device_id: id.to_string(),
                name: "qa".into(),
                paired_at: iso8601_now(),
                last_seen: None,
            };
            let res = sync.registry().lock().unwrap_or_else(|e| e.into_inner()).add(entry);
            match res {
                Ok(()) => ok(json!({ "ok": true })),
                Err(e) => Some(Response::error(req.id.clone(), INTERNAL_ERROR, e.to_string())),
            }
        }
        "qa_append" => {
            let pane = match pane_param(req) {
                Ok(p) => p,
                Err(r) => return Some(r),
            };
            let data = req
                .params
                .get("data")
                .and_then(|v| v.as_str())
                .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok());
            let Some(data) = data else { return Some(bad(req, "data must be base64")) };
            match sm.process_pane_bytes(PaneId(pane), &data) {
                Ok(_) => ok(json!({ "ok": true })),
                Err(e) => Some(Response::error(req.id.clone(), INTERNAL_ERROR, e.to_string())),
            }
        }
        "qa_truncate_log_before" => {
            let pane = match pane_param(req) {
                Ok(p) => p,
                Err(r) => return Some(r),
            };
            let Some(offset) = req.params.get("offset").and_then(|v| v.as_u64()) else {
                return Some(bad(req, "offset is required"));
            };
            let sm2 = sm.clone();
            tokio::spawn(async move {
                sm2.truncate_log_before(PaneId(pane), offset).await;
            });
            // Return once the log really starts at `offset` (the disk part runs
            // on the appender task), so a reconnect right after cannot race it.
            let t0 = std::time::Instant::now();
            while sm.log_bounds(PaneId(pane)).is_some_and(|(first, _)| first < offset)
                && t0.elapsed() < std::time::Duration::from_secs(2)
            {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            ok(json!({ "ok": true }))
        }
        "qa_read_log" => {
            let pane = match pane_param(req) {
                Ok(p) => p,
                Err(r) => return Some(r),
            };
            let Some((first, total)) = sm.log_bounds(PaneId(pane)) else {
                return Some(bad(req, "pane not found"));
            };
            let mut data = Vec::new();
            let mut next = first;
            while next < total && data.len() < READ_CAP {
                match sm.plan_log(PaneId(pane), next, 1 << 20) {
                    Some(LogPlan::Ring(b)) => {
                        next += b.len() as u64;
                        data.extend_from_slice(&b);
                    }
                    Some(LogPlan::Disk { disk, len }) => {
                        // Blocking read: this is a test method on a blocking thread.
                        use std::io::{Read, Seek, SeekFrom};
                        let path = sm.log_shared(PaneId(pane)).map(|s| s.path.clone());
                        let want = (len as u64).min(disk.flushed_to.saturating_sub(next)) as usize;
                        let read = path.and_then(|p| {
                            let mut f = std::fs::File::open(p).ok()?;
                            f.seek(SeekFrom::Start(next - disk.first)).ok()?;
                            let mut buf = vec![0u8; want];
                            f.read_exact(&mut buf).ok()?;
                            Some(buf)
                        });
                        match read {
                            Some(b) if !b.is_empty() => {
                                next += b.len() as u64;
                                data.extend_from_slice(&b);
                            }
                            _ => break,
                        }
                    }
                    _ => break,
                }
            }
            ok(json!({
                "first": first, "total": total,
                "data": base64::engine::general_purpose::STANDARD.encode(&data),
            }))
        }
        "qa_input" => {
            let pane = match pane_param(req) {
                Ok(p) => p,
                Err(r) => return Some(r),
            };
            let data = rpc
                .qa
                .input_log
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&pane)
                .cloned()
                .unwrap_or_default();
            ok(json!({ "data": base64::engine::general_purpose::STANDARD.encode(&data) }))
        }
        "qa_push_event" => {
            match req.params.get("event").cloned().map(serde_json::from_value::<ServerEvent>) {
                Some(Ok(event)) => {
                    rpc.emit(event);
                    ok(json!({ "ok": true }))
                }
                Some(Err(e)) => Some(bad(req, &format!("bad event: {e}"))),
                None => Some(bad(req, "event is required")),
            }
        }
        "qa_fire_events" => {
            // `count` LayoutChanged events with the current layout, as fast as
            // the hub takes them (load for the stalled-reader test).
            let count = req.params.get("count").and_then(|v| v.as_u64()).unwrap_or(1);
            let layout = rpc.layout_value();
            for _ in 0..count {
                rpc.emit(ServerEvent::LayoutChanged { layout: layout.clone() });
            }
            ok(json!({ "ok": true, "fired": count }))
        }
        "qa_force_catchup" => {
            // Every stream of the pane takes a snapshot catch-up now.
            let pane = match pane_param(req) {
                Ok(p) => p,
                Err(r) => return Some(r),
            };
            rpc.qa.force_catchup.send_modify(|(p, n)| {
                *p = pane;
                *n += 1;
            });
            ok(json!({ "ok": true }))
        }
        "qa_push_status" => {
            let pane = match pane_param(req) {
                Ok(p) => p,
                Err(r) => return Some(r),
            };
            let s = |k: &str| req.params.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
            rpc.emit(ServerEvent::StatusChanged {
                pane_id: pane,
                status: s("status"),
                message: s("message"),
            });
            ok(json!({ "ok": true }))
        }
        _ => None,
    }
}
