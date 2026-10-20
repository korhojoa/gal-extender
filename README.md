<!-- SPDX-License-Identifier: GPL-3.0-only -->
# gal-extender

## What it does

- Show Android Auto map video on the instrument cluster.
- Optionally send vehicle sensor data to Android Auto, including an EV energy
  model when you supply the model file.
- Include optional key input, a cluster settings menu, wireless Android Auto,
  a vendor data channel and a command launcher. These optional features are parked and
  disabled in the example configuration.

Rust extensions for the Android Auto receiver on QNX 6.5 ARM.
The loader uses the head unit's existing interfaces. This repository contains
no receiver, firmware or QNX development kit.

See [Terms](docs/TERMS.md) for the technical names and
[Settings](docs/SETTINGS.md) for configuration files and values.

## Components

- `gal-loader` builds `libgal_loader.so`. It loads the enabled built-in modules.
- `gal-cluster-video` sends Android Auto video to the cluster.
- `gal-sensor-inject` supplies sensor values through the published `exlap` client.
- `gal-feature-api` supplies shared hooks, configuration access and lifecycle calls.
- `gal-vendor-abi` declares the receiver's native interfaces.
- `most-isotx2` supplies the MOST transport writer.
- `aa-wire-patch` changes supported Android Auto message fields.
- `protobuf-wire` reads and writes protocol fields.
- `mib2-shared-crates` supplies shared video and settings code.
- `qnx650-crates` supplies QNX runtime interfaces.

Cluster, sensor, input, uplink, wireless transport, launcher and menu modules
are built in. Wireless code is compiled but disabled in the example. Use
[the example configuration](examples/gal_loader.conf) for this build. The loader
logs one warning per unavailable bare feature name in the startup configuration
and continues with the available features. Duplicate names produce one warning.
The loader does not load external feature libraries.

[Runtime interfaces](docs/INTERFACES.md) describes the native interface boundary.
Symbol names, offsets and control codes are interface facts. They are not
firmware implementations.

## Build

Install Git and Podman. The build script starts the compiler container and
fetches the locked Rust dependencies. No Rust installation or QNX development
kit is required on the host.

```sh
git clone --recurse-submodules https://github.com/korhojoa/gal-extender.git
cd gal-extender
./build.sh
```

To use Docker instead of Podman, run `CONTAINER_ENGINE=docker ./build.sh`.
The compiler image is fixed by digest in `compiler-image.txt`. The workflow
uses the same image and build steps. The build checks formatting, Clippy and
host tests, then builds the ARM libraries and checks the loader ELF header
and runtime dependencies. One root Cargo workspace and lock file cover all crates.
The libraries are `target/armv7-unknown-nto-qnx650/release/libgal_loader.so`
and `target/armv7-unknown-nto-qnx650/release/libgal_btshim.so`.
Set `CARGO_TARGET_DIR` to use another build directory. Registry downloads use
`CARGO_HOME`, or `$HOME/.cargo` when it is unset. A container engine must be
running for the build. The build does not install software on a head unit.

## Installation

Use the [combined installation guide](https://github.com/korhojoa/mib2-extenders/blob/main/INSTALL.md).
Put `gal_loader.conf` beside `libgal_loader.so`. The loader finds its directory
through the dynamic linker or its absolute preload path. It does not use a
fixed card slot or an alternate configuration path. `GAL_LOADER_DIR` is an
absolute test override. If the directory or configuration cannot be found,
the loader enables no features and logs the reason.

For `cluster_output=2`, put the decoder at `dio-extender/cluster-video`
beside the `gal-extender` folder. The loader passes `port=5320` to this
helper and sends CVS1 data over the local connection. The default output
uses MOST directly and does not start the helper. This optional decoder route
has no recorded unit result from GAL.

## Configuration

Use [Settings](docs/SETTINGS.md) for every loader key, its type, limits and
code default. It also describes the configuration files and supplied EV data.
Start with [the example](examples/gal_loader.conf), then preserve existing
settings during an update. The example values can differ from code defaults.

## Status

Tested on a MIB2 High unit, firmware MHI2_ER_VWG13_P4512_2, 2026-10-20.

The Android Auto map appeared on the first connection. There were no cluster
stream write errors. Native navigation returned after disconnect. The sensor
feature sends battery level from EXLAP. Other physical sensor mappings and
speed or position injection remain unqualified.

| Feature | Status |
| --- | --- |
| Direct cluster video | Passed the unit test above. |
| Sensor battery level | Uses EXLAP. Other sensor mappings need separate tests. |
| User-supplied EV model | Needs a suitable model file. No model is included. |
| Navigation guidance decoder | Disabled. Maneuver mapping needs correction. |
| Cluster video through the DIO decoder | Parked. No recorded unit result for this GAL route. |
| Input and cluster menu | Parked. Disabled in the example. |
| Vendor uplink | Parked. Needs a service and phone application that are not released. |
| Wireless TCP transport | Parked. Needs a phone relay that is not released. |
| Bluetooth bootstrap | Parked. Needs credential and policy writers that are not released. No recorded unit result. |
| Command launcher | Parked. Disabled in the example. |

The loader waits for the native encoder to stop before it allows direct
cluster output. It also applies handover and panel-settle checks. These
checks do not qualify another firmware or an untested display route.

## Licenses

| Source | SPDX license |
| --- | --- |
| GAL loader and native extension glue | `GPL-3.0-only` |
| `aa-wire-patch` and `protobuf-wire` | `MIT OR Apache-2.0` |
| `mib2-shared-crates` | `MIT OR Apache-2.0` |
| `qnx650-crates` | As declared in each crate: `Apache-2.0` or `MIT OR Apache-2.0` |
| Published `exlap` dependency | `MIT OR Apache-2.0` |

Each source file and package retains its license identifier. See
[COPYING](COPYING) for GPL version 3, [MIT](LICENSES/MIT.txt) and
[Apache-2.0](LICENSES/Apache-2.0.txt) for the shared-code licenses. The submodules
retain their license texts. Registry dependencies retain their package notices.
Product names identify compatible systems and do not imply endorsement.
