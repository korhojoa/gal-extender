// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Native method interposers use stack-aligning ARM entry trampolines.
//! Stack-word counts describe arguments after registers r0 through r3.

use oldabi_entry::entry;

use crate::{codec, gal, nav_focus, rx, usbdi};

// gal's methods. Stack words: see each signature in gal.rs / rx.rs.
entry!("_ZN9VideoSink16addDiscoveryInfoEP24ServiceDiscoveryResponse" => gal::video_sink_add_discovery_info, 0);
entry!("_ZN12SensorSource16addDiscoveryInfoEP24ServiceDiscoveryResponse" => gal::sensor_source_add_discovery_info, 0);
entry!("_ZN11InputSource16addDiscoveryInfoEP24ServiceDiscoveryResponse" => gal::input_source_add_discovery_info, 0);
entry!("_ZN11GalReceiver15registerServiceEP20ProtocolEndpointBase" => gal::gal_receiver_register_service, 0);
entry!("_ZN13MessageRouter19notifyChannelOpenedEhRK18ChannelOpenRequest" => gal::message_router_notify_channel_opened, 0);
entry!("_ZN9VideoSink23handleVideoFocusRequestERK29VideoFocusRequestNotification" => gal::video_sink_handle_video_focus_request, 0);
entry!("_ZN9VideoSink13setVideoFocusEib" => gal::video_sink_set_video_focus, 0);
entry!("_ZN7Channel15enqueueOutgoingERK10shared_ptrI5FrameE" => gal::channel_enqueue_outgoing, 0);
// (this, long long ts in r2:r3, bool b on the stack)
entry!("_ZN10Controller15sendPingRequestExb" => gal::controller_send_ping_request, 1);
entry!("_ZN10Controller18handlePingResponseERK12PingResponse" => gal::controller_handle_ping_response, 0);
entry!("_ZN10Controller17handlePingRequestERK11PingRequest" => gal::controller_handle_ping_request, 0);
entry!("_ZN10Controller17sendByeByeRequestE12ByeByeReason" => gal::controller_send_byebye_request, 0);
entry!("_ZN10Controller18sendByeByeResponseEv" => gal::controller_send_byebye_response, 0);
entry!("_ZN12SensorSource19handleSensorRequestERK13SensorRequest" => gal::sensor_source_handle_sensor_request, 0);
// (this, u64 ts in r2:r3, then kc, down, b2, m on the stack)
entry!("_ZN11InputSource7sendKeyEyjbbj" => gal::input_source_send_key, 4);
// (this, u64 ts in r2:r3, then kc, x on the stack)
entry!("_ZN11InputSource18reportKeyLongPressEyjj" => gal::input_source_report_key_long_press, 2);
// (this + 6 words: 3 in r1-r3, 3 on the stack)
entry!("_ZN12SensorSource14reportFuelDataEbibibb" => gal::sensor_source_report_fuel_data, 3);
// (this + 5 words: 3 in r1-r3, 2 on the stack)
entry!("_ZN12SensorSource15reportSpeedDataEibbbi" => gal::sensor_source_report_speed_data, 2);
entry!("_ZN9Transport10readOrFailEPvj" => gal::transport_read_or_fail, 0);
entry!("_ZN9Transport11writeOrFailEPvj" => gal::transport_write_or_fail, 0);
entry!("_ZN11GalReceiver15prepareShutdownEv" => gal::gal_receiver_prepare_shutdown, 0);
entry!("_ZN11GalReceiver8shutdownEv" => gal::gal_receiver_shutdown, 0);
entry!("_ZN13MessageRouter21sendUnexpectedMessageEh" => rx::message_router_send_unexpected_message, 0);
entry!("_ZN13MessageRouter24queueOutgoingUnencryptedEhPvj" => rx::message_router_queue_outgoing_unencrypted, 0);
entry!("_ZN13MessageRouter12routeMessageEhRK10shared_ptrI8IoBufferE" => rx::message_router_route_message, 0);

entry!("_ZN10Controller12routeMessageEhtRK10shared_ptrI8IoBufferE" => nav_focus::controller_route_message, 0);

// Native destructor registration for the receiver configuration table. see codec.rs.
entry!("__aeabi_atexit" => codec::aeabi_atexit, 0);

// libusbdi
entry!("usbd_connect" => usbdi::usbd_connect, 0);
entry!("usbd_attach" => usbdi::usbd_attach, 0);
// (dev, root, type, index in r0-r3, node** on the stack)
entry!("usbd_parse_descriptors" => usbdi::usbd_parse_descriptors, 1);
entry!("usbd_open_pipe" => usbdi::usbd_open_pipe, 0);
entry!("usbd_topology_ext" => usbdi::usbd_topology_ext, 0);
