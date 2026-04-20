use std::{
    collections::{HashMap, HashSet},
    net::Ipv4Addr,
    path::PathBuf,
};

use clap::Parser;
use dune_core::cfg::{Config, Defaults, Endpoint, Link, Node, Phynode, Phynodes, Topology};
use toml::Value;
use tracing::{error, info};
use tracing_subscriber;

const GW_ID: u32 = 65535;
const MAX_METRIC: u16 = 65534;

#[derive(Parser)]
struct Args {
    code: String,
    latencies: PathBuf,
}

#[derive(Debug)]
enum Walker {
    Delta,
    Star,
}

enum WalkerError {
    Unsupported,
}

impl TryFrom<&str> for Walker {
    type Error = WalkerError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if value == "D" {
            Ok(Walker::Delta)
        } else if value == "S" {
            Ok(Walker::Star)
        } else {
            Err(WalkerError::Unsupported)
        }
    }
}

enum CodeError {
    CodeMalformed,
    Walker(WalkerError),
    AltitudeUnparsable,
    InclinationUnparsable,
    PlaneParameters(PlaneParametersError),
}

#[derive(Debug)]
struct Code {
    walker: Walker,
    altitude: f64,
    inclination: f64,
    plane_parameters: PlaneParameters,
}

#[derive(Debug)]
struct PlaneParameters {
    sats: u32,
    planes: u32,
    phasing: u32,
}

enum PlaneParametersError {
    Malformed,
    SatsUnparsable,
    PlanesUnparsable,
    PhasingFactorUnparsable,
}

impl TryFrom<&str> for PlaneParameters {
    type Error = PlaneParametersError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let parts: Vec<&str> = value.split('/').collect();
        if parts.len() != 3 {
            return Err(PlaneParametersError::Malformed);
        }

        let sats = str::parse::<u32>(parts[0]).map_err(|_| PlaneParametersError::SatsUnparsable)?;
        let planes =
            str::parse::<u32>(parts[1]).map_err(|_| PlaneParametersError::PlanesUnparsable)?;
        let phasing = str::parse::<u32>(parts[2])
            .map_err(|_| PlaneParametersError::PhasingFactorUnparsable)?;

        Ok(PlaneParameters {
            sats,
            planes,
            phasing,
        })
    }
}

impl TryFrom<String> for Code {
    type Error = CodeError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let parts: Vec<&str> = value.split(':').collect();
        if parts.len() < 4 {
            return Err(CodeError::CodeMalformed);
        }

        let walker = Walker::try_from(parts[0]).map_err(|err| CodeError::Walker(err))?;
        let altitude = str::parse::<f64>(parts[1]).map_err(|_| CodeError::AltitudeUnparsable)?;
        let inclination =
            str::parse::<f64>(parts[2]).map_err(|_| CodeError::InclinationUnparsable)?;
        let plane_parameters =
            PlaneParameters::try_from(parts[3]).map_err(|err| CodeError::PlaneParameters(err))?;
        Ok(Code {
            walker,
            altitude,
            inclination,
            plane_parameters,
        })
    }
}

#[derive(Debug)]
struct LinkLatency {
    head: Option<(u32, u32)>,
    gw: Option<String>,
    tail: (u32, u32),
    latency: f64,
    enabled: bool,
}

use regex::Regex;

impl TryFrom<&str> for LinkLatency {
    type Error = ();
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        // head(plane, rank) tail(plane, rank) latency
        if let Ok(re) = Regex::new(r"\(([0-9]+),([0-9]+)\) \(([0-9]+),([0-9]+)\) ([0-9]+\.[0-9]+)")
            && let Some(line) = re.captures(value)
        {
            let (_, [head_plane, head_rank, tail_plane, tail_rank, latency]) = line.extract();
            let head: (u32, u32) = (
                str::parse::<u32>(head_plane).map_err(|_| ())?,
                str::parse::<u32>(head_rank).map_err(|_| ())?,
            );
            let tail: (u32, u32) = (
                str::parse::<u32>(tail_plane).map_err(|_| ())?,
                str::parse::<u32>(tail_rank).map_err(|_| ())?,
            );
            let latency = str::parse::<f64>(latency).map_err(|_| ())?;
            return Ok(LinkLatency {
                head: Some(head),
                gw: None,
                tail,
                latency,
                enabled: true,
            });

        // gw tail(plane, rank) latency
        } else if let Ok(re) =
            Regex::new(r"(gw[0-9]*) (add|del) \(([0-9]+), ([0-9]+)\) ([0-9]+\.[0-9]+)")
            && let Some(line) = re.captures(value)
        {
            let (_, [gw, enable, tail_plane, tail_rank, latency]) = line.extract();

            let tail: (u32, u32) = (
                str::parse::<u32>(tail_plane).map_err(|_| ())?,
                str::parse::<u32>(tail_rank).map_err(|_| ())?,
            );
            let latency = str::parse::<f64>(latency).map_err(|_| ())?;
            return Ok(LinkLatency {
                head: None,
                gw: Some(gw.to_string()),
                tail,
                latency,
                enabled: (enable == "del"),
            });
        }
        Err(())
    }
}

fn coord_to_id(plane: u32, rank: u32, n_sat: u32) -> u32 {
    plane * n_sat + rank
}

fn id_to_iface(rid: u32, ifaces: &mut HashMap<u32, u32>) -> String {
    let iface_id = ifaces.entry(rid).or_default();
    let iface = format!("eth{iface_id}");
    *iface_id += 1;
    iface
}

fn coord_to_endpoint(
    plane: u32,
    rank: u32,
    n_sat: u32,
    ifaces: &mut HashMap<u32, u32>,
) -> Endpoint {
    let node: String = format!("r{plane}_{rank}").into();
    let id = coord_to_id(plane, rank, n_sat);
    let interface: String = id_to_iface(id, ifaces);
    Endpoint { node, interface }
}

fn gw_to_endpoint(node: String, ifaces: &mut HashMap<u32, u32>) -> Endpoint {
    let interface = id_to_iface(GW_ID, ifaces);
    Endpoint { node, interface }
}

fn main() {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    println!("{:?}", args.code);

    // let mut nodes: HashMap<String, Node> = Default::default();
    // let mut links: Vec<Link> = Default::default();

    // // Map each node to its next usable interface id.
    // let mut ifaces: HashMap<u32, u32> = Default::default();

    // let latencies: Option<(f64, Vec<LinkLatency>, Vec<LinkLatency>, Vec<LinkLatency>)> =
    //     if args.latencies.exists()
    //         && args.latencies.is_file()
    //         && let Ok(latencies) = std::fs::read(&args.latencies)
    //         && let Ok(latencies) = String::from_utf8(latencies)
    //     {
    //         let mut latencies = latencies.split('\n');

    //         let _handover_ts = latencies.next();

    //         let ip_latency = latencies
    //             .next()
    //             .and_then(|l| str::parse::<f64>(l).ok())
    //             .unwrap();

    //         // TODO: Accept multiple add and multiple deletes.
    //         let add = latencies
    //             .next()
    //             .and_then(|line| LinkLatency::try_from(line).ok())
    //             .unwrap();

    //         let del = latencies
    //             .next()
    //             .and_then(|line| LinkLatency::try_from(line).ok())
    //             .unwrap();

    //         let cp_latencies: Vec<LinkLatency> = latencies
    //             .filter_map(|line| LinkLatency::try_from(line).ok())
    //             .collect();
    //         Some((ip_latency, vec![add], vec![del], cp_latencies))
    //     } else {
    //         error!("Failed to load latencies.");
    //         None
    //     };

    // if let Ok(code) = Code::try_from(args.code)
    //     && let Some((ip_latency, adds, dels, cp_latencies)) = latencies
    // {
    //     let sat_per_plane = code.plane_parameters.sats / code.plane_parameters.planes;
    //     info!(?code, ?sat_per_plane);

    //     // Add nodes and in-plane links.
    //     (0..code.plane_parameters.planes).for_each(|plane| {
    //         (0..sat_per_plane).for_each(|rank| {
    //             let linklat = LinkLatency {
    //                 head: Some((plane, rank)),
    //                 gw: None,
    //                 tail: (plane, (rank + 1) % sat_per_plane),
    //                 latency: ip_latency,
    //                 enabled: true,
    //             };
    //             let link = linklat_to_link(&linklat, sat_per_plane, &mut ifaces).unwrap();

    //             // Save mapping node:rid
    //             let head_id = coord_to_id(plane, rank, sat_per_plane);
    //             nodes.entry(link.endpoints[0].node.clone()).or_insert({
    //                 let mut node: Node = Default::default();
    //                 let id = Ipv4Addr::from_bits(head_id);
    //                 node._additional_fields = Some(HashMap::from_iter(vec![(
    //                     "rid".to_string(),
    //                     Value::String(id.to_string()),
    //                 )]));
    //                 node
    //             });

    //             links.push(link);
    //         })
    //     });

    //     // Add cross-plane links.
    //     let mut pairs: HashSet<(u32, u32)> = Default::default();
    //     cp_latencies
    //         .iter()
    //         .filter(|linklat| {
    //             let head = linklat.head.unwrap();
    //             let head_id = coord_to_id(head.0, head.1, sat_per_plane);
    //             let tail_id = coord_to_id(linklat.tail.0, linklat.tail.1, sat_per_plane);
    //             if !pairs.contains(&(head_id, tail_id)) && !pairs.contains(&(tail_id, head_id)) {
    //                 pairs.insert((head_id, tail_id));
    //                 true
    //             } else {
    //                 false
    //             }
    //         })
    //         .for_each(|linklat| {
    //             links.push(linklat_to_link(linklat, sat_per_plane, &mut ifaces).unwrap());
    //         });

    //     adds.iter().chain(dels.iter()).for_each(|linklat| {
    //         // Add gateways if not already stored.
    //         if let Some(gw) = &linklat.gw
    //             && !nodes.contains_key(gw)
    //         {
    //             let mut node = Node::default();
    //             node._additional_fields = Some(HashMap::from_iter(vec![(
    //                 "rid".to_string(),
    //                 Value::String(Ipv4Addr::from_bits(GW_ID).to_string()),
    //             )]));
    //             nodes.insert(gw.clone(), node);
    //         }

    //         // Add link.
    //         links.push(linklat_to_link(linklat, sat_per_plane, &mut ifaces).unwrap());
    //     });

    //     let topology = Topology {
    //         defaults: Defaults {
    //             links: None,
    //             nodes: None,
    //         },
    //         nodes,
    //         links,
    //     };

    //     let phynode = Phynode {
    //         cores: vec![vec![0]],
    //         binds: Default::default(),
    //         _additional_fields: Default::default(),
    //     };

    //     let cfg = Config {
    //         infrastructure: Phynodes {
    //             nodes: HashMap::from_iter(vec![("a".to_string(), phynode)]),
    //             _additional_fields: Default::default(),
    //         },
    //         topology,
    //     };

    //     let mut dst = args.latencies.clone();
    //     dst.set_extension("toml");
    //     if let Ok(contents) = toml::to_string(&cfg) {
    //         let _ = std::fs::write(dst, contents.as_bytes());
    //     }
    // }
}
