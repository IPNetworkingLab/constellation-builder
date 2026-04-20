use std::{
    collections::{HashMap, HashSet},
    net::Ipv4Addr,
    path::PathBuf,
};

use dune_core::cfg::{Config, Defaults, Endpoint, Link, Node, Phynode, Phynodes, Topology};
use toml::Value;
// use tracing::{error, info};

use constellation::{
    CrossPlaneLatencies, ShellCoordinates, Timestamp, gateway::Gateway, walker::WalkerShellVariant,
};
use constellation_code::Code;
use handover_filter::{EmitHandover, HandoverType};
use tracing::{debug, warn};

pub type Latencies = CrossPlaneLatencies;
pub type Interfaces = HashMap<usize, usize>;

// FIXME: Compute id from constellation code
pub const GW_ID: u32 = 65535;
pub const MAX_METRIC: u16 = 65534;

#[derive(Debug)]
pub struct LinkLatency {
    pub head: Option<ShellCoordinates>,
    pub gw: Option<String>,
    pub tail: ShellCoordinates,
    pub latency: f64,
    enabled: bool,
}

fn id_to_iface(rid: u32, ifaces: &mut Interfaces) -> String {
    let iface_id = ifaces.entry(rid as usize).or_default();
    let iface = format!("eth{iface_id}");
    *iface_id += 1;
    iface
}

fn coordinates_to_endpoint(
    code: &Code,
    coords: &ShellCoordinates,
    ifaces: &mut Interfaces,
) -> Result<Endpoint, ()> {
    let (plane, rank) = *coords;
    let node: String = format!("r{plane}_{rank}").into();
    let id = code.coordinates_to_id(plane, rank).map_err(|_| ())?;
    let interface: String = id_to_iface(id as u32, ifaces);
    Ok(Endpoint { node, interface })
}

fn gw_to_endpoint(node: String, ifaces: &mut Interfaces) -> Endpoint {
    let interface = id_to_iface(GW_ID, ifaces);
    Endpoint { node, interface }
}

fn linklat_to_link(code: &Code, value: &LinkLatency, ifaces: &mut Interfaces) -> Result<Link, ()> {
    let head_endpoint = if let Some(coords) = value.head {
        coordinates_to_endpoint(code, &coords, ifaces)?
    } else if let Some(gw) = &value.gw {
        gw_to_endpoint(gw.clone(), ifaces)
    } else {
        return Err(());
    };

    let tail_endpoint = coordinates_to_endpoint(code, &value.tail, ifaces)?;

    let metric_value = Value::Integer((value.latency * 100f64) as i64);

    let mut fields: Vec<(String, Value)> =
        vec![("latency".to_string(), Value::Float(value.latency))];

    let metrics: Vec<(String, Value)> = if value.gw.is_some() {
        vec![
            (
                format!("{}:{}", head_endpoint.node, head_endpoint.interface).to_string(),
                Value::Table(toml::map::Map::from_iter(vec![(
                    "metric".to_string(),
                    Value::Integer(MAX_METRIC.into()),
                )])),
            ),
            (
                format!("{}:{}", tail_endpoint.node, tail_endpoint.interface).to_string(),
                Value::Table(toml::map::Map::from_iter(vec![(
                    "metric".to_string(),
                    if value.enabled {
                        metric_value
                    } else {
                        Value::Integer(MAX_METRIC.into())
                    },
                )])),
            ),
        ]
    } else {
        vec![("metric".to_string(), metric_value)]
    };
    fields.extend(metrics);

    Ok(Link {
        endpoints: [head_endpoint, tail_endpoint],
        _additional_fields: HashMap::from_iter(fields),
    })
}

pub type MetricChange = (u32, u32, u16, bool);

pub fn build<'a>(
    shell: &WalkerShellVariant,
    gw: &Gateway,
    ts: Timestamp,
    event: &EmitHandover<'a>,
    dump: Option<PathBuf>,
) -> Result<(Config, Vec<MetricChange>), ()> {
    let code = shell.code();
    let sats_per_plane = code.sats_per_plane();

    let cp_latencies = shell.get_cross_plane_latencies();
    let ip_latency = shell.get_in_plane_latency();

    let mut nodes: HashMap<String, Node> = Default::default();
    let mut ifaces: Interfaces = Default::default();
    let mut links: Vec<Link> = Default::default();

    // Add nodes and in-plane links.
    (0..code.plane_parameters.planes).for_each(|plane| {
        let plane = plane as usize;
        (0..sats_per_plane).for_each(|rank| {
            let linklat = LinkLatency {
                head: Some((plane, rank)),
                gw: None,
                tail: (plane, (rank + 1).rem_euclid(sats_per_plane)),
                latency: ip_latency,
                enabled: true,
            };
            let link = linklat_to_link(&code, &linklat, &mut ifaces).unwrap();

            // Save mapping node:rid
            // FIXME: Remove unwrap
            let head_id = code.coordinates_to_id(plane as usize, rank).unwrap();
            nodes.entry(link.endpoints[0].node.clone()).or_insert({
                let mut node: Node = Default::default();
                let id = Ipv4Addr::from_bits(head_id as u32);
                node._additional_fields = Some(HashMap::from_iter(vec![(
                    "rid".to_string(),
                    Value::String(id.to_string()),
                )]));
                node
            });
            links.push(link);
        })
    });

    // Add cross-plane links.
    let mut pairs: HashSet<(usize, usize)> = Default::default();
    cp_latencies
        .iter()
        .filter(|((head, tail), _)| {
            // FIXME: Handle unwrap
            let head_id = code.coordinates_to_id(head.0, head.1).unwrap();
            let tail_id = code.coordinates_to_id(tail.0, tail.1).unwrap();
            if !pairs.contains(&(head_id, tail_id)) && !pairs.contains(&(tail_id, head_id)) {
                pairs.insert((head_id, tail_id));
                true
            } else {
                false
            }
        })
        .for_each(|(&(head, tail), &latency)| {
            let linklat = LinkLatency {
                head: Some(head),
                gw: None,
                tail,
                latency,
                enabled: true,
            };
            links.push(linklat_to_link(&code, &linklat, &mut ifaces).unwrap());
        });

    fn get_linklat(
        gw: &Gateway,
        coords: ShellCoordinates,
        shell: &WalkerShellVariant,
        event_type: &HandoverType,
        ts: Timestamp,
    ) -> Option<(LinkLatency, MetricChange)> {
        let sat = shell.get_sat(coords).unwrap();
        gw.latency(&sat, ts).and_then(|latency| {
            let code = shell.code();
            let sat_id = code.coordinates_to_id(coords.0, coords.1).unwrap();
            let metric = match event_type {
                HandoverType::Addition => (latency * 100f64) as u16,
                HandoverType::Removal => MAX_METRIC,
            };
            let metric_change = (
                sat_id as u32,
                GW_ID,
                metric,
                matches!(event_type, HandoverType::Addition),
            );

            Some((
                LinkLatency {
                    gw: Some(gw.get_name().clone()),
                    head: None,
                    tail: coords,
                    latency,
                    enabled: matches!(event_type, HandoverType::Removal),
                },
                metric_change,
            ))
        })
    }
    // Add gateways if not already stored.
    if !nodes.contains_key(gw.get_name()) {
        let mut node = Node::default();
        node._additional_fields = Some(HashMap::from_iter(vec![(
            "rid".to_string(),
            Value::String(Ipv4Addr::from_bits(GW_ID).to_string()),
        )]));
        nodes.insert(gw.get_name().clone(), node);
    }

    // Add links from gw.
    let mut changes: Vec<MetricChange> = Default::default();
    for entry in match *event {
        EmitHandover::Handover {
            sat_addition,
            sat_removal,
            ..
        } => {
            vec![
                get_linklat(gw, sat_addition.into(), shell, &HandoverType::Addition, ts),
                get_linklat(gw, sat_removal.into(), shell, &HandoverType::Removal, ts),
            ]
        }
        EmitHandover::ConnectivityEvent {
            sat, event_type, ..
        } => {
            vec![get_linklat(gw, sat.into(), shell, event_type, ts)]
        }
    } {
        // Add link.
        if let Some((linklat, metric_change)) = entry {
            links.push(linklat_to_link(&code, &linklat, &mut ifaces).unwrap());
            changes.push(metric_change);
        } else {
            // warn!("No latency found!");
            return Err(());
        }
    }

    let topology = Topology {
        defaults: Defaults {
            links: None,
            nodes: None,
        },
        nodes,
        links,
    };

    let phynode = Phynode {
        cores: vec![vec![0]],
        binds: Default::default(),
        _additional_fields: Default::default(),
    };

    let cfg = Config {
        infrastructure: Phynodes {
            nodes: HashMap::from_iter(vec![("a".to_string(), phynode)]),
            _additional_fields: Default::default(),
        },
        topology,
    };

    if let Some(mut path) = dump {
        debug!("Topology dump requested");
        path.push("topology");
        if !path.exists() {
            let _ = std::fs::create_dir(path.clone());
        }
        let mut filename = format!("{ts:05}_{}", gw.get_name());
        for (src, _, _, add) in &changes {
            filename = format!("{filename}_{}_{src}", if *add { "add" } else { "del" });
        }
        path.push(filename);
        path.set_extension("toml");
        if let Ok(contents) = toml::to_string(&cfg) {
            if let Ok(_) = std::fs::write(path, contents.as_bytes()) {
                debug!("Toplogy file dumped.");
            } else {
                warn!("Failed to dump topology file.");
            }
        }
    }

    Ok((cfg, changes))
}
