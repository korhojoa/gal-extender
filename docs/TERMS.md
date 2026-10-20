<!-- SPDX-License-Identifier: GPL-3.0-only -->
# Terms

| Term | Meaning |
| --- | --- |
| QNX | The operating system used by the head unit. |
| ARM | The processor architecture used for these libraries. |
| Loader | The library loaded into GAL to install the enabled extension modules. |
| Podman or Docker | A container engine that runs the fixed compiler environment. |
| Parked | Available in the build but disabled in the example and not qualified for use. |
| TCP | The network byte-stream protocol used by the optional wireless receiver. |
| Protobuf | A binary message format with numbered, typed fields. |
| Varint | An integer encoded in a variable number of bytes in a protobuf message. |
| DPI | Dots per inch, the pixel-density setting advertised to the phone. |
| FPS | Frames per second, the video update rate. |
| AAC-LC | The low-complexity Advanced Audio Coding audio format. |
| ADTS | The framing format used to carry AAC audio packets. |
| Head unit | The vehicle computer that runs the main display and phone connection. |
| MIB2 High | The Harman infotainment platform supported by these native interfaces. |
| GAL or `gal` | The Android Auto receiver process on the head unit. |
| `dio_manager` | The separate CarPlay receiver process on the head unit. |
| Cluster | The instrument display behind the steering wheel. |
| MOST | The vehicle multimedia network used to send cluster video. |
| isoTX2 | The QNX device interface for an isochronous MOST transmit channel. |
| EXLAP | The head unit protocol used to read named vehicle measurements. |
| PPS | QNX Persistent Publish/Subscribe, a filesystem interface for shared attributes. |
| DSI | Data Service Interface, the message interface between infotainment services. |
| MMX | The multimedia processor subsystem that runs the phone receiver. |
| HMI | Human-machine interface, the software that presents native menus and displays. |
| CVS1 | The local video framing protocol used between the loader and the sibling decoder. |
| MPE | The native media encoder that produces the original cluster video. |
| Display manager context | A numbered native display scene that can be selected or hidden. |
| Encoder-off gate | A check that native video encoding has stopped before another writer starts. |
| Handover | The transfer of cluster output from native navigation to phone video. |
| Slog | The QNX system log used for runtime diagnostics. |
| H.264 | The video compression format used for cluster pictures. |
| IDR | An H.264 picture that allows decoding without earlier pictures. |
| EV | Electric vehicle. |
| Clippy | The Rust source linter used by the build. |
| ELF | The binary file format checked for the ARM libraries. |


- PCM is uncompressed digital audio represented by sampled values.
- SPS is an H.264 sequence parameter set with video format fields.
- A circuit breaker stops output after configured error limits are reached.

[README](../README.md)
