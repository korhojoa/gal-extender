<!-- SPDX-License-Identifier: GPL-3.0-only -->
# Runtime interfaces

This document describes native interfaces. The source resolves functions in
the head unit. The release contains no vendor executable or instruction copy.

## GAL loader

`gal-loader` exports the symbols listed in `crates/gal-loader/exports.txt`.
`gal-vendor-abi` defines typed function pointers and object offsets.
The loader forwards through the original function where its hook contract
requires it. A resolved symbol does not prove that a firmware version
supports a feature.

## Video and display

The shared [`h264-ts-mux`](../third_party/mib2-shared-crates/h264-ts-mux)
crate constructs MPEG transport packets.
[`display-manager-control`](../third_party/mib2-shared-crates/display-manager-control) calls the unit's display library through resolved
interfaces. A configuration value or a successful call does not prove that
the cluster shows a picture. Compilation does not qualify a display or phone session.

## Sensor protocol

The project uses the published `exlap` crate as a dependency. Sensor names
and field definitions describe the interface. Captured vehicle data is not
a release input.
