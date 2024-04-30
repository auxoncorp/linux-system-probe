use auxon_sdk::api::{Nanoseconds, TimelineId};
use procfs::{net::UdpState, process::ProcState, ticks_per_second};
use serde::{Deserialize, Serialize};
use std::{
    collections::{btree_map, BTreeMap},
    net::SocketAddr,
    time::Duration,
};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Config {
    pub poll_interval: Option<f32>,
    pub process_name_whitelist: Option<Vec<String>>,
}

impl Config {
    fn accept_process(&self, name: &str) -> bool {
        match &self.process_name_whitelist {
            Some(allowed_names) => {
                for allowed_name in allowed_names.iter() {
                    if name == allowed_name {
                        return true;
                    }
                }
                false
            }
            None => true,
        }
    }
}

#[derive(Default)]
struct ProbeState {
    pids: BTreeMap<i32, ProcessState>,
}

struct ProcessState {
    timeline_id: TimelineId,
    ordering: u128,
}

impl ProcessState {
    fn new() -> Self {
        Self {
            timeline_id: TimelineId::allocate(),
            ordering: 0,
        }
    }
}

#[tokio::main]
async fn main() {
    match do_main().await {
        Ok(()) => (),
        Err(e) => {
            eprintln!("{e}");
            let mut cause = e.source();
            while let Some(err) = cause {
                eprintln!("Caused by: {err}");
                cause = err.source();
            }
            std::process::exit(exitcode::SOFTWARE);
        }
    }
}

async fn do_main() -> Result<(), Box<dyn std::error::Error>> {
    auxon_sdk::init_tracing!();
    let cfg = auxon_sdk::plugin_utils::ingest::Config::<Config>::load("LINUX_PROBE_")?;
    let mut client = cfg.connect_and_authenticate().await?;
    let mut state = ProbeState::default();

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    let interval = tokio::time::interval(Duration::from_secs_f32(
        cfg.plugin.poll_interval.unwrap_or(1.0),
    ));
    tokio::pin!(interval);

    loop {
        tokio::select! {
            _ = &mut ctrl_c => {
                tracing::debug!("User signaled shutdown");
                return Ok(());
            },
            _ = interval.tick() => {
                send_data(&mut client, &cfg.plugin, &mut state).await?;
            }
        }
    }
}

async fn send_data(
    client: &mut auxon_sdk::plugin_utils::ingest::Client,
    cfg: &Config,
    state: &mut ProbeState,
) -> Result<(), Box<dyn std::error::Error>> {
    for proc in procfs::process::all_processes().unwrap() {
        let Ok(proc) = proc else { continue };
        let Ok(stat) = proc.stat() else { continue };

        if !cfg.accept_process(&stat.comm) {
            continue;
        }

        let mut ordering = match state.pids.entry(stat.pid) {
            btree_map::Entry::Vacant(v) => {
                let process_state = ProcessState::new();
                client.switch_timeline(process_state.timeline_id).await?;
                let state = v.insert(process_state);

                client
                    .send_timeline_attrs(&stat.comm, [("timeline.pid", stat.pid.into())])
                    .await?;
                state.ordering
            }

            btree_map::Entry::Occupied(mut o) => {
                let state = o.get_mut();
                client.switch_timeline(state.timeline_id).await?;
                let o = state.ordering;
                state.ordering += 1;
                o
            }
        };

        client
            .send_event(
                "stats",
                ordering,
                [
                    (
                        "process_state",
                        DisplayProcState(ProcState::from_char(stat.state).unwrap())
                            .to_string()
                            .into(),
                    ),
                    ("vsize", stat.vsize.into()),
                    ("rss", stat.rss.into()),
                    ("nice", stat.nice.into()),
                    ("user_time", ticks_to_ns(stat.utime).into()),
                    ("kernel_time", ticks_to_ns(stat.stime).into()),
                ],
            )
            .await?;

        if let Ok(udp_sockets) = proc.udp() {
            for u in udp_sockets {
                if u.state == UdpState::Close {
                    continue;
                }

                let (local_ip, local_port) = match u.local_address {
                    SocketAddr::V4(addr) => (addr.ip().to_string(), addr.port()),
                    SocketAddr::V6(addr) => (addr.ip().to_string(), addr.port()),
                };

                let (remote_ip, remote_port) = match u.remote_address {
                    SocketAddr::V4(addr) => (addr.ip().to_string(), addr.port()),
                    SocketAddr::V6(addr) => (addr.ip().to_string(), addr.port()),
                };

                ordering += 1;
                client
                    .send_event(
                        "udp",
                        ordering,
                        [
                            ("local_address", u.local_address.to_string().into()),
                            ("local_ip", local_ip.into()),
                            ("local_port", local_port.into()),
                            ("remote_address", u.remote_address.to_string().into()),
                            ("remote_ip", remote_ip.into()),
                            ("remote_port", remote_port.into()),
                            ("rx_queue", u.rx_queue.into()),
                            ("tx_queue", u.rx_queue.into()),
                            ("uid", u.uid.into()),
                            ("inode", u.inode.into()),
                        ],
                    )
                    .await?;
            }
        }

        // SAFETY: we initialized it above
        state.pids.get_mut(&stat.pid).unwrap().ordering = ordering;
    }

    Ok(())
}

fn ticks_to_ns(ticks: u64) -> Nanoseconds {
    (((ticks as f64 / ticks_per_second() as f64) * 1.0e9f64) as u64).into()
}

struct DisplayProcState(ProcState);
impl std::fmt::Display for DisplayProcState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self.0 {
            ProcState::Running => "Running",
            ProcState::Sleeping => "Sleeping",
            ProcState::Waiting => "Waiting",
            ProcState::Zombie => "Zombie",
            ProcState::Stopped => "Stopped",
            ProcState::Tracing => "Tracing",
            ProcState::Dead => "Dead",
            ProcState::Wakekill => "Wakekill",
            ProcState::Waking => "Waking",
            ProcState::Parked => "Parked",
            ProcState::Idle => "Idle",
        };
        f.write_str(s)
    }
}
