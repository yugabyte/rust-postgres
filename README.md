# YugabyteDB Rust-Postgres Smart Driver

This is a Rust driver for YugabyteDB SQL. This driver is based on [sfackler/rust-postgres](https://github.com/sfackler/rust-postgres), with the following additional connection properties:

- `load_balance`   - It expects **true/false** as its possible values.
- `topology_keys`  - It takes a comma separated geo-location values. A single geo-location can be given as 'cloud.region.zone'. Multiple geo-locations too can be specified, separated by comma (`,`).
- `yb_servers_refresh_interval` - (default value: 300 seconds) Time interval, in seconds, between two attempts to refresh the information about cluster nodes. Valid values are integers between 0 and 600. Value 0 means refresh for each connection request. Any value outside this range is ignored and the default is used.
- `fallback_to_topology_keys_only` : (default value: false) Applicable only for TopologyAware Load Balancing. When set to true, the smart driver does not attempt to connect to servers outside of primary and fallback placements specified via property. The default behaviour is to fallback to any available server in the entire cluster.
- `failed_host_reconnect_delay_secs` - (default value: 5 seconds) The driver marks a server as failed with a timestamp, when it cannot connect to it. Later, whenever it refreshes the server list via yb_servers(), if it sees the failed server in the response, it marks the server as UP only if failed-host-reconnect-delay-secs time has elapsed.

## Connection load balancing

Users can use this feature in two configurations.

### Cluster-aware / Uniform connection load balancing

In the cluster-aware connection load balancing, connections are distributed across all the tservers in the cluster, irrespective of their placements.

To enable the cluster-aware connection load balancing, provide the parameter `load_balance` set to true as `load_balance=true` in the connection url.

```
"postgresql://127.0.0.1:5433/yugabyte?user=yugabyte&password=yugabyte&load_balance=true"
```

With this parameter specified in the url, the driver will fetch and maintain the list of tservers from the given endpoint (`127.0.0.1` in above example) available in the YugabyteDB cluster and distribute the connections equally across them.

This list is refreshed every 5 minutes, when a new connection request is received.

Application needs to use the same connection url to create every connection it needs, so that the distribution happens equally.

### Topology-aware connection load balancing

With topology-aware connnection load balancing, users can target tservers in specific zones by specifying these zones as `topology_keys` with values in the format `cloudname.regionname.zonename`. Multiple zones can be specified as comma separated values.

The connections will be distributed equally with the tservers in these zones.

Note that, you would still need to specify `load_balance=true` to enable the topology-aware connection load balancing.

```
"postgresql://127.0.0.1:5433/yugabyte?user=yugabyte&password=yugabyte&load_balance=true&topology_keys=cloud1.datacenter1.rack1"
```
### Specifying fallback zones

For topology-aware load balancing, you can now specify fallback placements too. This is not applicable for cluster-aware load balancing.
Each placement value can be suffixed with a colon (`:`) followed by a preference value between 1 and 10.
A preference value of `:1` means it is a primary placement. A preference value of `:2` means it is the first fallback placement and so on. If no preference value is provided, it is considered to be a primary placement (equivalent to one with preference value `:1`). Example given below.

```
"postgresql://127.0.0.1:5433/yugabyte?user=yugabyte&password=yugabyte&load_balance=true&topology_keys=cloud1.region1.zone1:1,cloud1.region1.zone2:2";
```

You can also use `*` for specifying all the zones in a given region as shown below. This is not allowed for cloud or region values.

```
"postgresql://127.0.0.1:5433/yugabyte?user=yugabyte&password=yugabyte&load_balance=true&topology_keys=cloud1.region1.*:1,cloud1.region2.*:2";
```

The driver attempts to connect to a node in following order: the least loaded node in the 1) primary placement(s), else in the 2) first fallback if specified, else in the 3) second fallback if specified and so on.
If no nodes are available either in primary placement(s) or in any of the fallback placements, then nodes in the rest of the cluster are attempted.
And this repeats for each connection request.

## Specifying Refresh Interval

Users can specify Refresh Time Interval, in seconds. It is the time interval between two attempts to refresh the information about cluster nodes. Default is 300. Valid values are integers between 0 and 600. Value 0 means refresh for each connection request. Any value outside this range is ignored and the default is used.

To specify Refresh Interval, use the parameter `yb_servers_refresh_interval` in the connection url.
```
"postgresql://127.0.0.1:5433/yugabyte?user=yugabyte&password=yugabyte&yb_servers_refresh_interval=0&load_balance=true&topology_keys=cloud1.region1.*:1,cloud1.region2.*:2";
```

For working examples which demonstrates both the configurations of connection load balancing using `tokio-postgres::connect()`, see the [driver-examples](https://github.com/yugabyte/driver-examples/tree/main/rust/rust_ysql_driver_examples) repository.

PostgreSQL support for Rust.

## postgres [![Latest Version](https://img.shields.io/crates/v/postgres.svg)](https://crates.io/crates/postgres)

[Documentation](https://docs.rs/postgres)

A native, synchronous PostgreSQL client.

## tokio-postgres [![Latest Version](https://img.shields.io/crates/v/tokio-postgres.svg)](https://crates.io/crates/tokio-postgres)

[Documentation](https://docs.rs/tokio-postgres)

A native, asynchronous PostgreSQL client.

## postgres-types [![Latest Version](https://img.shields.io/crates/v/postgres-types.svg)](https://crates.io/crates/postgres-types)

[Documentation](https://docs.rs/postgres-types)

Conversions between Rust and Postgres types.

## postgres-native-tls [![Latest Version](https://img.shields.io/crates/v/postgres-native-tls.svg)](https://crates.io/crates/postgres-native-tls)

[Documentation](https://docs.rs/postgres-native-tls)

TLS support for postgres and tokio-postgres via native-tls.

## postgres-openssl [![Latest Version](https://img.shields.io/crates/v/postgres-openssl.svg)](https://crates.io/crates/postgres-openssl)

[Documentation](https://docs.rs/postgres-openssl)

TLS support for postgres and tokio-postgres via openssl.

# Running test suite

The test suite requires postgres to be running in the correct configuration. The easiest way to do this is with docker:

1. Install `docker` and `docker-compose`.
   1. On ubuntu: `sudo apt install docker.io docker-compose`.
1. Make sure your user has permissions for docker.
   1. On ubuntu: ``sudo usermod -aG docker $USER``
1. Change to top-level directory of `rust-postgres` repo.
1. Run `docker-compose up -d`.
1. Run `cargo test`.
1. Run `docker-compose stop`.
