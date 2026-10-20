<!-- SPDX-License-Identifier: GPL-3.0-only -->
# Settings

## Files and reader rules

Put `gal_loader.conf` beside `libgal_loader.so`. A missing file enables no
features. A missing key uses its code default. The supplied example selects
explicit values and can differ from those defaults.
Bare `cluster` and `sensor` lines enable those modules. Other feature names
are `input`, `ui`, `aauplink`, `wireless` and `launcher`. All are off when
absent. The last five are parked and off in the example.

The optional `android-auto.conf` and `android-auto.conf.backup` files contain
settings overrides. `extender_settings_dir` selects their folder. Without
that key, the reader uses the base configuration folder. Keep existing
settings during an update. The loader reads settings at process start.

The parked launcher reads `gal_launch.conf` beside the loader configuration.
It accepts up to 32 commands. `once: COMMAND` runs once after a successful
exit. `daemon: COMMAND` or a bare command starts only when it is not running.
`check=NAME COMMAND` checks that process name. Blank and comment lines are
ignored. Commands run through a shell. Child signal isolation is not
guaranteed. Do not enable the launcher without a suitable command list.

## Feature selectors

| Bare line | Type | Minimum | Maximum | Default | Meaning |
| --- | --- | --- | --- | --- | --- |
| `cluster` | Presence flag | Not applicable | Not applicable | Off | Enable cluster video. |
| `sensor` | Presence flag | Not applicable | Not applicable | Off | Enable EXLAP sensor integration. |
| `input` | Presence flag | Not applicable | Not applicable | Off | Enable parked input handling. |
| `ui` | Presence flag | Not applicable | Not applicable | Off | Enable the parked cluster menu. |
| `aauplink` | Presence flag | Not applicable | Not applicable | Off | Enable the parked vendor channel. Its service and phone application are not released. |
| `wireless` | Presence flag | Not applicable | Not applicable | Off | Enable the parked TCP transport. Its phone relay is not released. |
| `launcher` | Presence flag | Not applicable | Not applicable | Off | Enable the parked command launcher. |

## Loader keys

All numeric keys use signed decimal integers. The first matching key wins.
The minimum and maximum below describe runtime input limits, not proof that
every value gives usable geometry or timing. Unless stated otherwise, a
switch uses 0 for off and any nonzero integer for on. Use 0 or 1.
The settings command tools can apply narrower edit limits than the loader.

| Key | Type | Minimum | Maximum | Default | Meaning |
| --- | --- | --- | --- | --- | --- |
| `aa_suppress_navigation_active` | Integer switch | -2147483648 | 2147483647 | `0` | Suppress the mobile-navigation-active message when nonzero. |
| `cluster_boot_settle_secs` | Signed 32-bit integer | -2147483648 | 2147483647 | `90` | Minimum unit uptime before cluster takeover when no handover release is available. |
| `cluster_cb_cooldown_secs` | Signed 32-bit integer | -2147483648 | 2147483647 | `60` | Time to leave the native screen active after a circuit breaker trip. |
| `cluster_cb_enable` | Integer switch | -2147483648 | 2147483647 | `1` | Stop cluster writes and restore the native screen after repeated write failures. |
| `cluster_cb_hard_stall_ms` | Signed 32-bit integer | -2147483648 | 2147483647 | `1000` | A single write blocked this long opens the circuit breaker. 0 disables this limit. |
| `cluster_cb_max_trips` | Signed 32-bit integer | -2147483648 | 2147483647 | `3` | Disable cluster output until the next phone session after this many circuit breaker trips. |
| `cluster_cb_max_werr` | Signed 32-bit integer | -2147483648 | 2147483647 | `6` | Number of write errors that opens the circuit breaker. |
| `cluster_cb_stall_count` | Signed 32-bit integer | -2147483648 | 2147483647 | `3` | Number of slow writes that opens the circuit breaker. |
| `cluster_cb_stall_ms` | Signed 32-bit integer | -2147483648 | 2147483647 | `400` | A write at least this slow counts toward the slow-write limit. |
| `cluster_codec_resolution` | Signed 32-bit integer | 1 | 2 | `1` | 1 selects 800x480. 2 selects 1280x720. Other values use 1. |
| `cluster_content_inset_bottom` | Signed 32-bit integer | 0 | 2147483647 | `0` | Area inside the video window reserved at the bottom, in pixels. |
| `cluster_content_inset_left` | Signed 32-bit integer | 0 | 2147483647 | `64` | Area inside the video window reserved at the left, in pixels. |
| `cluster_content_inset_right` | Signed 32-bit integer | 0 | 2147483647 | `64` | Area inside the video window reserved at the right, in pixels. |
| `cluster_content_inset_top` | Signed 32-bit integer | 0 | 2147483647 | `0` | Area inside the video window reserved at the top, in pixels. |
| `cluster_density` | Signed 32-bit integer | 0 | 2147483647 | `180` | Cluster pixel density. |
| `cluster_dm_display` | Signed 32-bit integer | -2147483648 | 2147483647 | `4` | Display manager display identifier. |
| `cluster_dm_hide_context` | Signed 32-bit integer | -2147483648 | 2147483647 | `-9` | Context used to hide native navigation. |
| `cluster_dm_table` | Signed 32-bit integer | -2147483648 | 2147483647 | `70` | Native context restored on release. |
| `cluster_flush_max_hold_ms` | Signed 32-bit integer | -2147483648 | 2147483647 | `100` | Maximum wait before padding and sending a partially filled MOST block. |
| `cluster_focus_kick` | Integer switch | -2147483648 | 2147483647 | `0` | Request a new keyframe after a queued-video backlog drains. |
| `cluster_focus_kick_grace_ms` | Signed 32-bit integer | -2147483648 | 2147483647 | `15000` | Minimum stream age before backlog recovery can request a keyframe. |
| `cluster_focus_kick_max` | Signed 32-bit integer | -2147483648 | 2147483647 | `3` | Maximum backlog recovery keyframe requests per phone session. |
| `cluster_focus_kick_min_ms` | Signed 32-bit integer | -2147483648 | 2147483647 | `20000` | Minimum time between backlog recovery keyframe requests. |
| `cluster_focus_kick_settle_ms` | Signed 32-bit integer | -2147483648 | 2147483647 | `1500` | Time the video queue must stay drained before requesting a keyframe. |
| `cluster_focus_kick_wq` | Signed 32-bit integer | -2147483648 | 2147483647 | `16` | Queue depth, in frames, that marks a backlog for keyframe recovery. |
| `cluster_gate_enable` | Integer switch | -2147483648 | 2147483647 | `1` | Wait for native encoder and handover state before allowing cluster output. |
| `cluster_height_margin` | Signed 32-bit integer | 0 | 2147483647 | `344` | Vertical layout margin in pixels. |
| `cluster_main_focus_extra_ms` | Signed 32-bit integer | -2147483648 | 2147483647 | `750` | Additional delay after the main screen reaches foreground before takeover. |
| `cluster_main_focus_wait_ms` | Signed 32-bit integer | -2147483648 | 2147483647 | `8000` | Maximum wait for the main screen to reach foreground. |
| `cluster_margin_bottom` | Signed 32-bit integer | -2147483648 | 2147483647 | `96` | Window margin at the bottom, in pixels. -1 divides the total margin equally. |
| `cluster_margin_left` | Signed 32-bit integer | -2147483648 | 2147483647 | `48` | Window margin at the left, in pixels. -1 divides the total margin equally. |
| `cluster_margin_right` | Signed 32-bit integer | -2147483648 | 2147483647 | `48` | Window margin at the right, in pixels. -1 divides the total margin equally. |
| `cluster_margin_top` | Signed 32-bit integer | -2147483648 | 2147483647 | `60` | Window margin at the top, in pixels. -1 divides the total margin equally. |
| `cluster_max_unacked` | Signed 32-bit integer | 1 | 64 | `1` | Maximum unacknowledged video messages, from 1 to 64. Other values use 1. |
| `cluster_output` | Signed 32-bit integer | 0 | 2 | `0` | 0 sends directly through MOST. 2 sends CVS1 to the DIO decoder. 1 is unsupported. Values other than 0 and 2 use 0. |
| `cluster_quiet_driver` | Integer switch | -2147483648 | 2147483647 | `1` | Disable repeated driver notifications during cluster writes. |
| `cluster_ready_settle_ms` | Signed 32-bit integer | -2147483648 | 2147483647 | `5000` | Wait after an uptime-based release before allowing cluster video. |
| `cluster_sps_crop` | Integer switch | -2147483648 | 2147483647 | `0` | Change H.264 crop parameters to the configured cluster content window. |
| `cluster_stable_inset_bottom` | Signed 32-bit integer | 0 | 2147483647 | `0` | Stable reserved area advertised to the phone at the bottom, in pixels. |
| `cluster_stable_inset_left` | Signed 32-bit integer | 0 | 2147483647 | `0` | Stable reserved area advertised to the phone at the left, in pixels. |
| `cluster_stable_inset_right` | Signed 32-bit integer | 0 | 2147483647 | `0` | Stable reserved area advertised to the phone at the right, in pixels. |
| `cluster_stable_inset_top` | Signed 32-bit integer | 0 | 2147483647 | `0` | Stable reserved area advertised to the phone at the top, in pixels. |
| `cluster_ui_pace_kbps` | Signed 32-bit integer | 0 | 2147483647 | `9600` | UI video write rate in kilobits per second. Negative values use 9600. |
| `cluster_viewing_distance` | Signed 32-bit integer | 0 | 2147483647 | `100` | Viewing distance in millimetres. |
| `cluster_width_margin` | Signed 32-bit integer | 0 | 2147483647 | `270` | Horizontal layout margin in pixels. |
| `cluster_write_pace_max_wait_ms` | Signed 32-bit integer | -2147483648 | 2147483647 | `1000` | Maximum wait for a write pacing delay. |
| `cluster_write_pace_pct` | Signed 32-bit integer | 0 | 2147483647 | `150` | 0 disables pacing. Otherwise use at least 100. Values 1 through 99 and negative values use 150. |
| `identify_as_dhu` | Integer switch | -2147483648 | 2147483647 | `0` | Present the unit as Google's Desktop Head Unit, an Android Auto development receiver. 0 preserves the native identity. |
| `main_dpi` | Signed 32-bit integer | 0 | 2147483647 | `0` | 0 keeps the native density. Only 1280x720 main-display offers are changed. A new value must keep the encoded integer width. Requires the cluster feature. |
| `main_fps` | Signed 32-bit integer | 0 | 30 | `0` | Only 0 and 30 are accepted. 0 keeps the native rate. Other values use 0. Requires the cluster feature. |
| `media_audio_codec` | Signed 32-bit integer | 0 | 4 | `0` | Only 0, 2 and 4 are accepted. 0 keeps the native PCM selection. 2 selects AAC-LC. 4 selects AAC-LC ADTS. Other values leave the native selection unchanged. Applies only to a matching native configuration table. |
| `sensor_cred` | Signed 32-bit integer | 0 | 3 | `2` | Initial EXLAP credential profile index. Values outside 0 through 3 use profile 2. The client cycles through the four profiles after failed sessions. |
| `sensor_ev_advertise` | Integer switch | -2147483648 | 2147483647 | `0` | Offer the electric-vehicle energy-model sensor, type 23, to the phone. |
| `sensor_ev_capacity_wh` | Signed 32-bit integer | -2147483648 | 2147483647 | `30000` | Vehicle-specific battery capacity in watt-hours. The example value can differ. Supply the capacity suitable for the vehicle. |
| `sensor_ev_send` | Integer switch | -2147483648 | 2147483647 | `0` | Send the electric-vehicle model from ev_model.bin with updated battery values. |
| `sensor_raw_ev_trip` | Integer switch | -2147483648 | 2147483647 | `0` | Offer sensor type 26 and send the opaque data from raw_ev_trip.bin. |
| `sensor_send_accel` | Integer switch | -2147483648 | 2147483647 | `0` | Offer and report accelerometer readings. |
| `sensor_send_compass` | Integer switch | -2147483648 | 2147483647 | `0` | Offer and report compass heading. |
| `sensor_send_dead_reckoning` | Integer switch | -2147483648 | 2147483647 | `0` | Offer and report motion readings used for dead reckoning. |
| `sensor_send_door` | Integer switch | -2147483648 | 2147483647 | `0` | Offer and report door state. |
| `sensor_send_environment` | Integer switch | -2147483648 | 2147483647 | `0` | Offer and report ambient environment readings. |
| `sensor_send_gyro` | Integer switch | -2147483648 | 2147483647 | `0` | Offer and report gyroscope readings. |
| `sensor_send_hvac` | Integer switch | -2147483648 | 2147483647 | `0` | Advertise the climate sensor type. No climate set-point sender is connected. Keep this disabled. |
| `sensor_send_light` | Integer switch | -2147483648 | 2147483647 | `0` | Offer and report exterior light state. |
| `sensor_send_parking_brake` | Integer switch | -2147483648 | 2147483647 | `1` | Report parking brake state on its existing sensor route. |
| `sensor_send_passenger` | Integer switch | -2147483648 | 2147483647 | `0` | Offer and report passenger state. |
| `sensor_send_rpm` | Integer switch | -2147483648 | 2147483647 | `0` | Offer and report engine speed in revolutions per minute. |
| `sensor_send_tire` | Integer switch | -2147483648 | 2147483647 | `0` | Offer and report tyre pressure readings. |
| `ui_encode_w` | Signed 32-bit integer | -2147483648 | 2147483647 | `800` | Parked cluster menu. Video width in pixels, rounded down to a multiple of 16. Values below 16 use 800. |
| `ui_encode_h` | Signed 32-bit integer | -2147483648 | 2147483647 | `480` | Parked cluster menu. Video height in pixels, rounded down to a multiple of 16. Values below 16 use 480. |
| `ui_keepalive_ms` | Signed 32-bit integer | -2147483648 | 2147483647 | `1500` | Parked cluster menu. Interval between repeated menu pictures in milliseconds. |
| `ui_idle_timeout_s` | Signed 32-bit integer | -2147483648 | 2147483647 | `45` | Parked cluster menu. Close the menu after this many seconds without a key event. |
| `ui_trigger_keycode` | Signed 32-bit integer | -2147483648 | 2147483647 | `84` | Parked cluster menu. Receiver key code used to open or close the menu. |
| `ui_remount` | Integer switch | -2147483648 | 2147483647 | `1` | Parked cluster menu. Permit temporary read-write remounting of the settings volume when saving menu changes. |
| `aauplink_router_preflight` | Integer switch | -2147483648 | 2147483647 | `1` | Parked vendor channel. Needs a service and phone application that are not released. Check the receiver channel registration before advertising the vendor uplink. |
| `aauplink_shm_recheck_ms` | Signed 32-bit integer | -2147483648 | 2147483647 | `5000` | Parked vendor channel. Needs a service and phone application that are not released. Interval in milliseconds before retrying the shared-memory ring connection. |
| `aauplink_flush_on_open` | Integer switch | -2147483648 | 2147483647 | `1` | Parked vendor channel. Needs a service and phone application that are not released. Discard queued phone-bound ring data when a channel opens. |
| `aauplink_flush_on_close` | Integer switch | -2147483648 | 2147483647 | `1` | Parked vendor channel. Needs a service and phone application that are not released. Discard queued phone-bound ring data when a channel closes. |
| `wireless_tcp_port` | Signed 32-bit integer | -2147483648 | 2147483647 | `0` | Parked TCP transport. Positive values start a listener after conversion to a 16-bit port. Use 1 through 65535. Nonpositive values can use the environment override. Needs a phone relay that is not released. |
| `extender_settings_dir` | Path text | Not applicable | Not applicable | configuration folder | Directory for settings overrides. |

Negative values for unsigned layout fields use the current code default.
The total width and height margins set the content window. Explicit top,
bottom, left and right margins position that window. A margin of -1 splits
the corresponding total equally. Content insets reserve areas inside that
window. Stable insets tell the phone which areas stay reserved. Density and
viewing distance change the layout offered to the phone, not the display size.
Do not treat these separate measurements as interchangeable.

`cluster_dm_display`, `cluster_dm_table` and `cluster_dm_hide_context` are
native display identifiers. Changing them requires a matching firmware
interface. They are not screen dimensions.

The focus-kick keys control keyframe requests during stream recovery. The
example enables this path with `cluster_focus_kick=1`. Its code default is 0.
This setting alone does not qualify recovery on a unit. The navigation
guidance decoder in `aa-wire-patch` is disabled because its maneuver mapping needs correction.

## User-supplied electric-vehicle data

You supply `ev_model.bin` and `raw_ev_trip.bin`. Neither
file is distributed in this repository or its build output. Put each file
beside `libgal_loader.so` if its route is enabled. These files are not part of
the QNX runtime or firmware, and there is no bundled vehicle template.

`ev_model.bin` uses the same format as the file that
[aa-proxy-rs](https://github.com/aa-proxy/aa-proxy-rs) reads as `ev_model.bin`.
Use a model file valid for aa-proxy-rs. No model file is distributed.

`ev_model.bin` is a protobuf sensor-event body, at most 16384 bytes, with a
length-delimited energy-model field 23. It does not include the two-byte
channel header. The sender copies the template and patches these fields:

| Field path | Encoding | Value |
| --- | --- | --- |
| `23/1/3/1` | varint | Battery energy in watt-hours. |
| `23/1/4/1` | varint | Battery capacity in watt-hours. |
| `23/1/7` | 32-bit floating point, little endian | Outside temperature in degrees Celsius. |

The template must reserve enough encoded width for the battery values.
A value that does not fit leaves that field unchanged and produces a log
message. Other model fields remain unchanged. EXLAP supplies live values.
It does not supply the template. Without this file, the energy model cannot
be sent. `sensor_ev_advertise` only offers the sensor type to the phone.

`raw_ev_trip.bin` contains at most 4096 bytes of opaque vehicle trip settings.
The sender places those bytes in `raw_ev_trip_settings.raw_data`, sensor
type 26, and adds the channel frame. It does not interpret the payload.
Keep `sensor_raw_ev_trip=0` unless you supply a payload suitable for the vehicle.

## Environment options

| Variable | Type | Minimum | Maximum | Default | Meaning |
| --- | --- | --- | --- | --- | --- |
| `GAL_LOADER_DIR` | Absolute folder path | Not applicable | Not applicable | Loaded library folder | Override the loader configuration folder for explicit test use. |
| `GAL_WIRELESS_TCP_PORT` | Signed decimal integer | -2147483648 | 2147483647 | Unset | Parked wireless listener port when the configuration port is nonpositive. Use 1 through 65535 to enable a listener. |
| `GAL_PPS_ROOT` | Folder path | Not applicable | Not applicable | `/ramdisk/pps` | Root for the extension's published state objects. |
