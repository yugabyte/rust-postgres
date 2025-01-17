use crate::client::{Addr, SocketConfig};
use crate::config::{Host, LoadBalanceHosts, TargetSessionAttrs};
use crate::connect_raw::connect_raw;
use crate::connect_socket::connect_socket;
use crate::tls::MakeTlsConnect;
use crate::{Client, Config, Connection, Error, NoTls, SimpleQueryMessage, Socket};
use futures_util::{future, pin_mut, Future, FutureExt, Stream};
use lazy_static::lazy_static;
use log::{debug, info};
use rand::seq::SliceRandom;
use rand::Rng;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::task::Poll;
use std::time::Instant;
use std::{cmp, io};
use tokio::net;
use tokio::sync::Mutex as TokioMutex;

lazy_static! {
    static ref CONNECTION_COUNT_MAP: Mutex<HashMap<Host, i64>> = {
        let mut m = HashMap::new();
        let host_list_primary = HOST_INFO_PRIMAY.lock().unwrap().clone();
        let host_list_rr = HOST_INFO_RR.lock().unwrap().clone();
        let host_list = [host_list_primary, host_list_rr].concat();
        let size = host_list.len();
        for i in 0..size {
            let host = host_list.get(i);
            if host.is_some() {
                m.insert(host.unwrap().clone(), 0);
            }
        }
        Mutex::new(m)
    };
    static ref LAST_TIME_META_DATA_FETCHED: TokioMutex<Instant> = {
        let m = Instant::now();
        TokioMutex::new(m)
    };
    static ref HOST_INFO_PRIMAY: Mutex<Vec<Host>> = {
        let m = Vec::new();
        Mutex::new(m)
    };
    static ref HOST_INFO_RR: Mutex<Vec<Host>> = {
        let m = Vec::new();
        Mutex::new(m)
    };
    static ref FAILED_HOSTS: Mutex<HashMap<Host, Instant>> = {
        let m = HashMap::new();
        Mutex::new(m)
    };
    pub(crate) static ref PLACEMENT_INFO_MAP_PRIMARY: Mutex<HashMap<String, Vec<Host>>> = {
        let m = HashMap::new();
        Mutex::new(m)
    };
    pub(crate) static ref PLACEMENT_INFO_MAP_RR: Mutex<HashMap<String, Vec<Host>>> = {
        let m = HashMap::new();
        Mutex::new(m)
    };
    static ref PUBLIC_HOST_MAP: Mutex<HashMap<Host, Host>> = {
        let m = HashMap::new();
        Mutex::new(m)
    };
    static ref HOST_TO_PORT_MAP: Mutex<HashMap<Host, u16>> = {
        let m = HashMap::new();
        Mutex::new(m)
    };
}

static USE_PUBLIC_IP: AtomicBool = AtomicBool::new(false);

pub async fn connect<T>(
    mut tls: T,
    config: &Config,
) -> Result<(Client, Connection<Socket, T::Stream>), Error>
where
    T: MakeTlsConnect<Socket>,
{
    if config.host.is_empty() && config.hostaddr.is_empty() {
        return Err(Error::config("both host and hostaddr are missing".into()));
    }

    if !config.host.is_empty()
        && !config.hostaddr.is_empty()
        && config.host.len() != config.hostaddr.len()
    {
        let msg = format!(
            "number of hosts ({}) is different from number of hostaddrs ({})",
            config.host.len(),
            config.hostaddr.len(),
        );
        return Err(Error::config(msg.into()));
    }

    // At this point, either one of the following two scenarios could happen:
    // (1) either config.host or config.hostaddr must be empty;
    // (2) if both config.host and config.hostaddr are NOT empty; their lengths must be equal.
    let num_hosts = cmp::max(config.host.len(), config.hostaddr.len());

    if config.port.len() > 1 && config.port.len() != num_hosts {
        return Err(Error::config("invalid number of ports".into()));
    }

    let mut indices = (0..num_hosts).collect::<Vec<_>>();
    if config.load_balance_hosts == LoadBalanceHosts::Random {
        indices.shuffle(&mut rand::thread_rng());
    }

    let mut error = None;
    for i in indices {
        let host = config.host.get(i);
        let hostaddr = config.hostaddr.get(i);
        let port = config
            .port
            .get(i)
            .or_else(|| config.port.first())
            .copied()
            .unwrap_or(5433);

        // The value of host is used as the hostname for TLS validation,
        let hostname = match host {
            Some(Host::Tcp(host)) => Some(host.clone()),
            // postgres doesn't support TLS over unix sockets, so the choice here doesn't matter
            #[cfg(unix)]
            Some(Host::Unix(_)) => None,
            None => None,
        };

        // Try to use the value of hostaddr to establish the TCP connection,
        // fallback to host if hostaddr is not present.
        let addr = match hostaddr {
            Some(ipaddr) => Host::Tcp(ipaddr.to_string()),
            None => host.cloned().unwrap(),
        };

        match connect_host(addr, hostname, port, &mut tls, config).await {
            Ok((client, connection)) => return Ok((client, connection)),
            Err(e) => error = Some(e),
        }
    }

    Err(error.unwrap())
}

pub async fn yb_connect<T>(
    mut tls: T,
    config: &Config,
) -> Result<(Client, Connection<Socket, T::Stream>), Error>
where
    T: MakeTlsConnect<Socket>,
{
    if config.host.is_empty() && config.hostaddr.is_empty() {
        return Err(Error::config("both host and hostaddr are missing".into()));
    }

    if !config.host.is_empty()
        && !config.hostaddr.is_empty()
        && config.host.len() != config.hostaddr.len()
    {
        let msg = format!(
            "number of hosts ({}) is different from number of hostaddrs ({})",
            config.host.len(),
            config.hostaddr.len(),
        );
        return Err(Error::config(msg.into()));
    }

    // At this point, either one of the following two scenarios could happen:
    // (1) either config.host or config.hostaddr must be empty;
    // (2) if both config.host and config.hostaddr are NOT empty; their lengths must be equal.
    let num_hosts = cmp::max(config.host.len(), config.hostaddr.len());

    if config.port.len() > 1 && config.port.len() != num_hosts {
        return Err(Error::config("invalid number of ports".into()));
    }

    if !check_and_refresh(config).await {
        return Err(Error::connect(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "could not create control connection",
        )));
    }

    let host_to_port_map = HOST_TO_PORT_MAP.lock().unwrap().clone();

    loop {
        let newhost = get_least_loaded_server(config);
        let mut host = match newhost {
            Ok(host) => host,
            Err(e) => {
                // Throw the error
                return Err(e);
            }
        };

        increase_connection_count(host.clone());

        //check if we are to use public hosts
        if USE_PUBLIC_IP.load(Ordering::SeqCst) {
            let public_host_map = PUBLIC_HOST_MAP.lock().unwrap().clone();
            let public_host = public_host_map.get(&host.clone());
            if public_host.is_none() {
                info!("Public host not available for private host {:?}, adding this to failed host list and trying another server", host.clone());
                decrease_connection_count(host.clone());
                add_to_failed_host_list(host.clone());
                continue;
            } else {
                host = public_host.unwrap().clone();
            }
        }

        let hostname = match host.clone() {
            Host::Tcp(host) => Some(host),
            // postgres doesn't support TLS over unix sockets, so the choice here doesn't matter
            #[cfg(unix)]
            Host::Unix(_) => None,
        };

        info!("Creating connection to {:?}", hostname.clone());
        match connect_host(
            host.clone(),
            hostname.clone(),
            host_to_port_map[&(host.clone())],
            &mut tls,
            config,
        )
        .await
        {
            Ok((client, connection)) => return Ok((client, connection)),
            Err(_e) => {
                info!("Not able to create connection to {:?}, adding it to failed host list and trying a different host.",  hostname.clone());
                decrease_connection_count(host.clone());
                add_to_failed_host_list(host);
            }
        }
    }
}

fn increase_connection_count(host: Host) {
    let mut conn_map = CONNECTION_COUNT_MAP.lock().unwrap();
    let count = conn_map.get(&host);
    if count.is_none() {
        conn_map.insert(host.clone(), 1);
        debug!("Increasing connection count for {:?} to 1", host.clone());
    } else {
        let mut conn_count: i64 = *count.unwrap();
        conn_count += 1;
        conn_map.insert(host.clone(), conn_count);
        debug!(
            "Increasing connection count for {:?} by one: {}",
            host.clone(),
            conn_count
        );
    }
}

pub(crate) fn decrease_connection_count(host: Host) {
    let mut conn_map = CONNECTION_COUNT_MAP.lock().unwrap();
    let count = conn_map.get(&host);
    if count.is_some() {
        let mut conn_count: i64 = *count.unwrap();
        if conn_count != 0 {
            conn_count -= 1;
            conn_map.insert(host.clone(), conn_count);
            debug!(
                "Decremented connection count for {:?} by one: {}",
                host.clone(),
                conn_count
            );
        }
    }
}

fn get_least_loaded_server(config: &Config) -> Result<Host, Error> {
    let conn_map = CONNECTION_COUNT_MAP.lock().unwrap().clone();
    let host_list_primary = HOST_INFO_PRIMAY.lock().unwrap().clone();
    let host_list_rr = HOST_INFO_RR.lock().unwrap().clone();
    let failed_host_list = FAILED_HOSTS.lock().unwrap().clone();
    let placement_info_map_primary = PLACEMENT_INFO_MAP_PRIMARY.lock().unwrap().clone();
    let placement_info_map_rr = PLACEMENT_INFO_MAP_RR.lock().unwrap().clone();
    let mut least_host: Vec<Host> = Vec::new();

    let mut host_list: Vec<Host> = Vec::new();
    let mut placement_info_map: HashMap<String, Vec<Host>> = HashMap::new();

    if config.load_balance == "only-rr" || config.load_balance == "prefer-rr" {
        host_list = host_list_rr.clone();
        placement_info_map = placement_info_map_rr.clone();
    } else if config.load_balance == "only-primary" || config.load_balance == "prefer-primary" {
        host_list = host_list_primary.clone();
        placement_info_map = placement_info_map_primary.clone();
    } else {
        host_list = host_list_rr.clone();
        placement_info_map = placement_info_map_rr.clone();
        host_list.extend(host_list_primary.clone());
        for (key, value) in placement_info_map_primary {
            if let Some(vec) = placement_info_map.get_mut(&key) {
                vec.extend(value);
            } else {
                placement_info_map.insert(key, value);
            }
        }
    }

    if !config.topology_keys.is_empty() {
        for i in 0..config.topology_keys.len() as i64 {
            let mut server: Vec<Host> = Vec::new();
            let prefered_zone = config.topology_keys.get(&(i + 1)).unwrap();
            for placement_info in prefered_zone.iter() {
                let to_check_star: Vec<&str> = placement_info.split(".").collect();
                if to_check_star[2] == "*" {
                    let star_placement_info: String =
                        to_check_star[0].to_owned() + "." + to_check_star[1];
                    let append_hosts = placement_info_map.get(&star_placement_info);
                    if let Some(append_hosts_value) = append_hosts {
                        server.extend(append_hosts_value.to_owned());
                    }
                } else {
                    let append_hosts = placement_info_map.get(placement_info);
                    if let Some(append_hosts_value) = append_hosts {
                        server.extend(append_hosts_value.to_owned());
                    }
                }
            }
            least_host = get_least_loaded_hosts(server, conn_map.clone(), failed_host_list.clone());

            if !least_host.is_empty() {
                break;
            }
        }
    }

    if least_host.is_empty() {
        if !(config.load_balance == "prefer-primary" || config.load_balance == "prefer-rr") {
            if config.topology_keys.is_empty() || !config.fallback_to_topology_keys_only {
                least_host = get_least_loaded_hosts(host_list, conn_map.clone(), failed_host_list.clone());
            } else {
                return Err(Error::connect(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "no preferred server available, fallback-to-topology-keys-only is set to true",
                )));
            }
        } else {
            least_host = get_least_loaded_hosts(host_list, conn_map.clone(), failed_host_list.clone());
            if least_host.is_empty() {
                if config.load_balance == "prefer-rr"{
                    least_host = get_least_loaded_hosts(host_list_primary, conn_map.clone(), failed_host_list.clone());
                } else {
                    least_host = get_least_loaded_hosts(host_list_rr, conn_map.clone(), failed_host_list.clone());
                }
            }
        }
    }

    if !least_host.is_empty() {
        info!(
            "Following hosts have the least number of connections: {:?}, chosing one randomly",
            least_host
        );
        let num = rand::thread_rng().gen_range(0..least_host.len());
        Ok(least_host.get(num).cloned().expect("least loaded host value is None"))
    } else {
        Err(Error::connect(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "could not find a server to connect to",
        )))
    }
}

fn get_least_loaded_hosts(hosts: Vec<Host>, conn_map: HashMap<Host, i64>, failed_hosts: HashMap<Host, Instant>) -> Vec<Host> {
    let mut min_count = i64::MAX;
    let mut least_host: Vec<Host> = Vec::new();
    for host in hosts.iter() {
        if !failed_hosts.contains_key(host) {
            let count = conn_map.get(host);
            let mut counter: i64 = 0;
            if count.is_some() {
                counter = *count.unwrap();
            }
            if min_count > counter {
                min_count = counter;
                least_host.clear();
                least_host.push(host.clone());
            } else if min_count == counter {
                least_host.push(host.clone());
            }
        }
    }
    least_host
}

async fn check_and_refresh(config: &Config) -> bool {
    let mut refresh_time = LAST_TIME_META_DATA_FETCHED.lock().await;
    let host_list_primary = HOST_INFO_PRIMAY.lock().unwrap().clone();
    let host_list_rr = HOST_INFO_RR.lock().unwrap().clone();
    let host_list = [host_list_primary, host_list_rr].concat();
    if host_list.is_empty() {
        info!("Connecting to the server for the first time");
        if let Ok((client, connection)) = connect(NoTls, config).await {
            let handle = tokio::spawn(async move {
                if let Err(e) = connection.await {
                    eprintln!("connection error: {}", e);
                }
            });
            info!("Control connection created to one of {:?}", config.host);
            refresh(client, config).await;
            let start = Instant::now();
            *refresh_time = start;
            info!("Resetting LAST_TIME_META_DATA_FETCHED");
            handle.abort();
            return true;
        } else {
            info!("Failed to establish control connection to available servers");
            return false;
        }
    } else {
        let duration = refresh_time.elapsed();
        if duration > config.yb_servers_refresh_interval {
            let host_to_port_map = HOST_TO_PORT_MAP.lock().unwrap().clone();
            let mut index = 0;
            while index < host_list.len() {
                let host = host_list.get(index);
                let mut conn_host = host.unwrap().to_owned();
                //check if we are to use public hosts
                if USE_PUBLIC_IP.load(Ordering::SeqCst) {
                    let public_host_map = PUBLIC_HOST_MAP.lock().unwrap().clone();
                    let public_host = public_host_map.get(&conn_host.clone());
                    if public_host.is_none() {
                        info!("Public host not available for private host {:?}, adding this to failed host list and trying another server", conn_host.clone());
                        add_to_failed_host_list(host.cloned().unwrap());
                        index += 1;
                        continue;
                    } else {
                        conn_host = public_host.unwrap().clone();
                    }
                }

                // The value of host is used as the hostname for TLS validation,
                let hostname = match conn_host.clone() {
                    Host::Tcp(host) => Some(host),
                    // postgres doesn't support TLS over unix sockets, so the choice here doesn't matter
                    #[cfg(unix)]
                    Host::Unix(_) => None,
                };

                if let Ok((client, connection)) = connect_host(
                    conn_host.clone(),
                    hostname.clone(),
                    host_to_port_map[&(conn_host.clone())],
                    &mut NoTls,
                    config,
                )
                .await
                {
                    let handle = tokio::spawn(async move {
                        if let Err(e) = connection.await {
                            eprintln!("connection error: {}", e);
                        }
                    });
                    info!("Control connection created to {:?}", hostname.clone());
                    refresh(client, config).await;
                    let start = Instant::now();
                    *refresh_time = start;
                    info!("Resetting LAST_TIME_META_DATA_FETCHED");
                    handle.abort();
                    return true;
                } else {
                    info!("Failed to establish control connection to {:?}, adding this to failed host list and trying another server", hostname.clone());
                    add_to_failed_host_list(host.cloned().unwrap());
                    index += 1;
                }
            }
            info!("Failed to establish control connection to available servers");
            return false;
        }
    }
    true
}

fn add_to_failed_host_list(host: Host) {
    let mut failedhostlist = FAILED_HOSTS.lock().unwrap();
    failedhostlist.insert(host.clone(), Instant::now());
    info!("Added {:?} to failed host list", host.clone());
}

async fn refresh(client: Client, config: &Config) {
    let socket_config = client.get_socket_config();
    let mut control_conn_host: String = String::new();
    if socket_config.is_some() {
        control_conn_host = socket_config.unwrap().hostname.unwrap();
    }

    info!("Executing query: `select * from yb_servers()` to fetch list of servers");
    let rows = client
        .query("select * from yb_servers()", &[])
        .await
        .unwrap();

    let mut host_list_primary = HOST_INFO_PRIMAY.lock().unwrap();
    let mut host_list_rr = HOST_INFO_RR.lock().unwrap();
    let mut failed_host_list = FAILED_HOSTS.lock().unwrap();
    let mut placement_info_map_primary = PLACEMENT_INFO_MAP_PRIMARY.lock().unwrap();
    let mut placement_info_map_rr = PLACEMENT_INFO_MAP_RR.lock().unwrap();
    let mut public_host_map = PUBLIC_HOST_MAP.lock().unwrap();
    let mut host_to_port_map = HOST_TO_PORT_MAP.lock().unwrap();
    for row in rows {
        let host_string: String = row.get("host");
        let host = Host::Tcp(host_string.to_string());
        info!("Received entry for host {:?}", host);
        let nodetype: String = row.get("node_type");
        let portvalue: i64 = row.get("port");
        let port: u16 = portvalue as u16;
        let cloud: String = row.get("cloud");
        let region: String = row.get("region");
        let zone: String = row.get("zone");
        let public_ip_string: String = row.get("public_ip");
        let public_ip = Host::Tcp(public_ip_string.to_string());
        let placement_zone: String = cloud.clone() + "." + &region + "." + &zone;
        let star_placement_zone: String = cloud.clone() + "." + &region;

        host_to_port_map.insert(host.clone(), port);
        host_to_port_map.insert(public_ip.clone(), port);

        if control_conn_host.eq_ignore_ascii_case(&public_ip_string) {
            USE_PUBLIC_IP.store(true, Ordering::SeqCst);
        }

        if !failed_host_list.contains_key(&host) {
            if nodetype == "primary" {
                if !host_list_primary.contains(&host) {
                    host_list_primary.push(host.clone());
                    public_host_map.insert(host.clone(), public_ip.clone());
                    debug!("Added {:?} to host list primary", host.clone());
                }
            } else {
                if !host_list_rr.contains(&host) {
                    host_list_rr.push(host.clone());
                    public_host_map.insert(host.clone(), public_ip.clone());
                    debug!("Added {:?} to host list RR", host.clone());
                }
            }
        } else {
            if failed_host_list.get(&host).unwrap().elapsed()
                > config.failed_host_reconnect_delay_secs
            {
                failed_host_list.remove(&host);
                debug!(
                    "Marking {:?} as UP since failed-host-reconnect-delay-secs has elapsed",
                    host.clone()
                );
                if nodetype == "primary" {
                    if !host_list_primary.contains(&host) {
                        host_list_primary.push(host.clone());
                        public_host_map.insert(host.clone(), public_ip.clone());
                        debug!("Added {:?} to host list primary", host.clone());
                    }
                } else {
                    if !host_list_rr.contains(&host) {
                        host_list_rr.push(host.clone());
                        public_host_map.insert(host.clone(), public_ip.clone());
                        debug!("Added {:?} to host list RR", host.clone());
                    }
                }
                make_connection_count_zero(host.clone());
            } else if host_list_primary.contains(&host) || host_list_rr.contains(&host) {
                debug!(
                    "Treating {:?} as DOWN since failed-host-reconnect-delay-secs has not elapsed",
                    host.clone()
                );
                if host_list_primary.contains(&host) {
                    let index = host_list_primary.iter().position(|x| *x == host).unwrap();
                    host_list_primary.remove(index);
                } else {
                    let index = host_list_rr.iter().position(|x| *x == host).unwrap();
                    host_list_rr.remove(index);
                }
                public_host_map.remove(&host);
            }
        }

        if nodetype == "primary" {
            if placement_info_map_primary.contains_key(&placement_zone) {
                let mut present_hosts = placement_info_map_primary.get(&placement_zone).unwrap().to_vec();
                if !present_hosts.contains(&host) {
                    present_hosts.push(host.clone());
                    placement_info_map_primary.insert(placement_zone.clone(), present_hosts.to_vec());
                }
            } else {
                let mut host_vec: Vec<Host> = Vec::new();
                host_vec.push(host.clone());
                placement_info_map_primary.insert(placement_zone.clone(), host_vec);
            }

            if placement_info_map_primary.contains_key(&star_placement_zone) {
                let mut star_present_hosts = placement_info_map_primary
                    .get(&star_placement_zone)
                    .unwrap()
                    .to_vec();
                if !star_present_hosts.contains(&host) {
                    star_present_hosts.push(host.clone());
                    placement_info_map_primary.insert(star_placement_zone.clone(), star_present_hosts.to_vec());
                }
            } else {
                let mut star_host_vec: Vec<Host> = Vec::new();
                star_host_vec.push(host.clone());
                placement_info_map_primary.insert(star_placement_zone.clone(), star_host_vec);
            }
        } else {
            if placement_info_map_rr.contains_key(&placement_zone) {
                let mut present_hosts = placement_info_map_rr.get(&placement_zone).unwrap().to_vec();
                if !present_hosts.contains(&host) {
                    present_hosts.push(host.clone());
                    placement_info_map_rr.insert(placement_zone, present_hosts.to_vec());
                }
            } else {
                let mut host_vec: Vec<Host> = Vec::new();
                host_vec.push(host.clone());
                placement_info_map_rr.insert(placement_zone, host_vec);
            }

            if placement_info_map_rr.contains_key(&star_placement_zone) {
                let mut star_present_hosts = placement_info_map_rr
                    .get(&star_placement_zone)
                    .unwrap()
                    .to_vec();
                if !star_present_hosts.contains(&host) {
                    star_present_hosts.push(host.clone());
                    placement_info_map_rr.insert(star_placement_zone, star_present_hosts.to_vec());
                }
            } else {
                let mut star_host_vec: Vec<Host> = Vec::new();
                star_host_vec.push(host.clone());
                placement_info_map_rr.insert(star_placement_zone, star_host_vec);
            }
        }
    }
}

fn make_connection_count_zero(host: Host) {
    let mut conn_map = CONNECTION_COUNT_MAP.lock().unwrap();
    let count = conn_map.get(&host);
    if count.is_none() {
        return;
    }
    conn_map.insert(host.clone(), 0);
    debug!("Resetting connection count for {:?} to zero", host.clone());
}


async fn connect_host<T>(
    host: Host,
    hostname: Option<String>,
    port: u16,
    tls: &mut T,
    config: &Config,
) -> Result<(Client, Connection<Socket, T::Stream>), Error>
where
    T: MakeTlsConnect<Socket>,
{
    match host {
        Host::Tcp(host) => {
            let mut addrs = net::lookup_host((&*host, port))
                .await
                .map_err(Error::connect)?
                .collect::<Vec<_>>();

            if config.load_balance_hosts == LoadBalanceHosts::Random {
                addrs.shuffle(&mut rand::thread_rng());
            }

            let mut last_err = None;
            for addr in addrs {
                match connect_once(Addr::Tcp(addr.ip()), hostname.as_deref(), port, tls, config)
                    .await
                {
                    Ok(stream) => return Ok(stream),
                    Err(e) => {
                        last_err = Some(e);
                        continue;
                    }
                };
            }

            Err(last_err.unwrap_or_else(|| {
                Error::connect(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "could not resolve any addresses",
                ))
            }))
        }
        #[cfg(unix)]
        Host::Unix(path) => {
            connect_once(Addr::Unix(path), hostname.as_deref(), port, tls, config).await
        }
    }
}

async fn connect_once<T>(
    addr: Addr,
    hostname: Option<&str>,
    port: u16,
    tls: &mut T,
    config: &Config,
) -> Result<(Client, Connection<Socket, T::Stream>), Error>
where
    T: MakeTlsConnect<Socket>,
{
    let socket = connect_socket(
        &addr,
        port,
        config.connect_timeout,
        config.tcp_user_timeout,
        if config.keepalives {
            Some(&config.keepalive_config)
        } else {
            None
        },
    )
    .await?;

    let tls = tls
        .make_tls_connect(hostname.unwrap_or(""))
        .map_err(|e| Error::tls(e.into()))?;
    let has_hostname = hostname.is_some();
    let (mut client, mut connection) = connect_raw(socket, tls, has_hostname, config).await?;

    if let TargetSessionAttrs::ReadWrite = config.target_session_attrs {
        let rows = client.simple_query_raw("SHOW transaction_read_only");
        pin_mut!(rows);

        let rows = future::poll_fn(|cx| {
            if connection.poll_unpin(cx)?.is_ready() {
                return Poll::Ready(Err(Error::closed()));
            }

            rows.as_mut().poll(cx)
        })
        .await?;
        pin_mut!(rows);

        loop {
            let next = future::poll_fn(|cx| {
                if connection.poll_unpin(cx)?.is_ready() {
                    return Poll::Ready(Some(Err(Error::closed())));
                }

                rows.as_mut().poll_next(cx)
            });

            match next.await.transpose()? {
                Some(SimpleQueryMessage::Row(row)) => {
                    if row.try_get(0)? == Some("on") {
                        return Err(Error::connect(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "database does not allow writes",
                        )));
                    } else {
                        break;
                    }
                }
                Some(_) => {}
                None => return Err(Error::unexpected_message()),
            }
        }
    }

    client.set_socket_config(SocketConfig {
        addr,
        hostname: hostname.map(|s| s.to_string()),
        port,
        connect_timeout: config.connect_timeout,
        tcp_user_timeout: config.tcp_user_timeout,
        keepalive: if config.keepalives {
            Some(config.keepalive_config.clone())
        } else {
            None
        },
    });

    Ok((client, connection))
}
