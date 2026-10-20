// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Typed native symbol declarations for the receiver, protocol interfaces.
//! Receiver and protobuf objects come from the receiver libraries. These
//! declarations do not establish compatibility with an unknown firmware.

/// Calls `$m!` with the whole symbol table:
/// `$( $(#[doc])* ($NAME, "mangled", fn type) ),*`.
#[macro_export]
macro_rules! for_each_vendor_symbol {
    ($m:ident) => {
        $m! {
            // SensorSource::reportEnvironmentData(bool,int,bool,int,bool,int). Boolean
            // arguments occupy integer ABI words.
            (SS_REPORT_ENVIRONMENT, "_ZN12SensorSource21reportEnvironmentDataEbibibi",
                unsafe extern "C" fn($crate::Ptr, i32, i32, i32, i32, i32, i32)),
            /// `SensorSource::reportRpmData(int)`.
            (SS_REPORT_RPM, "_ZN12SensorSource13reportRpmDataEi",
                unsafe extern "C" fn($crate::Ptr, i32)),
            /// `SensorSource::reportFuelData(bool,int,bool,int,bool,bool)`. The loader
            /// also interposes this function.
            (SS_REPORT_FUEL, "_ZN12SensorSource14reportFuelDataEbibibb",
                unsafe extern "C" fn($crate::Ptr, i32, i32, i32, i32, i32, i32)),
            /// `SensorSource::reportTirePressureData(int const*,int)`.
            (SS_REPORT_TIRE_PRESSURE, "_ZN12SensorSource22reportTirePressureDataEPKii",
                unsafe extern "C" fn($crate::Ptr, *const i32, i32)),
            /// `SensorSource::reportAccelerometerData(bool,int,bool,int,bool,int)`.
            (SS_REPORT_ACCELEROMETER, "_ZN12SensorSource23reportAccelerometerDataEbibibi",
                unsafe extern "C" fn($crate::Ptr, i32, i32, i32, i32, i32, i32)),
            /// `SensorSource::reportDoorData(bool,bool,bool,bool,bool const*,int)`. The
            /// mangled pointer type is bool const*. The caller supplies a zero element
            /// count. nonempty array contents are not qualified.
            (SS_REPORT_DOOR, "_ZN12SensorSource14reportDoorDataEbbbbPKbi",
                unsafe extern "C" fn($crate::Ptr, i32, i32, i32, i32, *const u8, i32)),
            /// `SensorSource::reportLightData(bool,int,bool,int,bool,bool)`.
            (SS_REPORT_LIGHT, "_ZN12SensorSource15reportLightDataEbibibb",
                unsafe extern "C" fn($crate::Ptr, i32, i32, i32, i32, i32, i32)),
            /// `SensorSource::reportHvacData(bool,int,bool,int)`.
            (SS_REPORT_HVAC, "_ZN12SensorSource14reportHvacDataEbibi",
                unsafe extern "C" fn($crate::Ptr, i32, i32, i32, i32)),
            /// `SensorSource::reportParkingBrakeData(bool)`.
            (SS_REPORT_PARKING_BRAKE, "_ZN12SensorSource22reportParkingBrakeDataEb",
                unsafe extern "C" fn($crate::Ptr, i32)),
            /// `SensorSource::reportCompassData(int)`.
            (SS_REPORT_COMPASS, "_ZN12SensorSource17reportCompassDataEi",
                unsafe extern "C" fn($crate::Ptr, i32)),
            /// `SensorSource::reportDeadReckoningData(bool,int,int const*,int)`.
            (SS_REPORT_DEAD_RECKONING, "_ZN12SensorSource23reportDeadReckoningDataEbiPKii",
                unsafe extern "C" fn($crate::Ptr, i32, i32, *const i32, i32)),
            /// `SensorSource::reportGyroscopeData(bool,int,bool,int,bool,int)`.
            (SS_REPORT_GYROSCOPE, "_ZN12SensorSource19reportGyroscopeDataEbibibi",
                unsafe extern "C" fn($crate::Ptr, i32, i32, i32, i32, i32, i32)),
            /// `SensorSource::reportPassengerData(bool)`.
            (SS_REPORT_PASSENGER, "_ZN12SensorSource19reportPassengerDataEb",
                unsafe extern "C" fn($crate::Ptr, i32)),
            /// `SensorSource::sendSensorResponse(int)`. Status zero selects success.
            (SS_SEND_SENSOR_RESPONSE, "_ZN12SensorSource18sendSensorResponseEi",
                unsafe extern "C" fn($crate::Ptr, i32)),

            // Queue an outgoing receiver message. The caller checks its native integer
            // result.
            (MR_QUEUE_OUTGOING, "_ZN13MessageRouter13queueOutgoingEhPvj",
                unsafe extern "C" fn($crate::Ptr, u8, $crate::Ptr, u32) -> i32),
            /// Register an endpoint. Return nonzero on insertion. zero when its index
            /// is invalid or already occupied. No unregister operation is declared.
            (MR_REGISTER_SERVICE, "_ZN13MessageRouter15registerServiceEP20ProtocolEndpointBase",
                unsafe extern "C" fn($crate::Ptr, $crate::Ptr) -> i32),

            // Delete a ServiceDiscoveryResponse with its deleting destructor.
            (SDR_DELETING_DTOR, "_ZN24ServiceDiscoveryResponseD0Ev",
                unsafe extern "C" fn($crate::Ptr)),
            /// Merge a `ServiceDiscoveryResponse`. The native function returns no result.
            (SDR_MERGE_FROM, "_ZN24ServiceDiscoveryResponse9MergeFromERKS_",
                unsafe extern "C" fn($crate::Ptr, $crate::ConstPtr)),
            /// Parse protobuf bytes from a bounded caller array.
            (MSG_PARSE_FROM_ARRAY, "_ZN6google8protobuf11MessageLite14ParseFromArrayEPKvi",
                unsafe extern "C" fn($crate::Ptr, $crate::ConstPtr, i32) -> bool),
            /// `MessageLite`::`ParsePartialFromArray` permits unset required fields.
            /// `ParseFromArray` can reject such a `ServiceDiscoveryResponse`.
            (MSG_PARSE_PARTIAL_FROM_ARRAY,
                "_ZN6google8protobuf11MessageLite21ParsePartialFromArrayEPKvi",
                unsafe extern "C" fn($crate::Ptr, $crate::ConstPtr, i32) -> bool),
            /// Allocate a new `ServiceDiscoveryResponse` from its default instance.
            (SDR_NEW, "_ZNK24ServiceDiscoveryResponse3NewEv",
                unsafe extern "C" fn($crate::ConstPtr) -> $crate::Ptr),
            /// Obtain the static `ServiceDiscoveryResponse` default instance.
            (SDR_DEFAULT_INSTANCE, "_ZN24ServiceDiscoveryResponse16default_instanceEv",
                unsafe extern "C" fn() -> $crate::Ptr),
            /// Return the serialized response size.
            (SDR_BYTE_SIZE, "_ZNK24ServiceDiscoveryResponse8ByteSizeEv",
                unsafe extern "C" fn($crate::ConstPtr) -> i32),
            /// Serialize a protobuf message into the caller array.
            (MSG_SERIALIZE_TO_ARRAY, "_ZNK6google8protobuf11MessageLite16SerializeToArrayEPvi",
                unsafe extern "C" fn($crate::ConstPtr, $crate::Ptr, i32) -> bool),


        }
    };
}

macro_rules! define_symbols {
    ($( $(#[$doc:meta])* ($name:ident, $mangled:literal, $ty:ty) ),* $(,)?) => {
        $(
            $(#[$doc])*
            // SAFETY: the type is the native function type cited in the doc above.
            pub const $name: $crate::Symbol<$ty> =
                unsafe { $crate::Symbol::new($crate::__cstr!($mangled)) };
        )*

        /// Every mangled name in the table, in table order.
        pub const ALL: &[&::core::ffi::CStr] = &[$( $crate::__cstr!($mangled) ),*];
    };
}

for_each_vendor_symbol!(define_symbols);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_complete_and_unique() {
        // 14 SensorSource + 2 MessageRouter + 8 SDR/protobuf.
        assert_eq!(ALL.len(), 24);
        for (i, a) in ALL.iter().enumerate() {
            assert!(a.to_bytes().starts_with(b"_Z"));
            assert!(ALL[i + 1..].iter().all(|b| b != a), "duplicate {a:?}");
        }
        assert_eq!(
            MR_REGISTER_SERVICE.name().to_bytes(),
            b"_ZN13MessageRouter15registerServiceEP20ProtocolEndpointBase"
        );
    }

    #[test]
    fn types_are_function_pointers() {
        fn size_of_val<F>(_: &crate::Symbol<F>) -> usize {
            core::mem::size_of::<F>()
        }
        assert_eq!(size_of_val(&SS_REPORT_FUEL), core::mem::size_of::<usize>());
        assert_eq!(
            size_of_val(&MSG_SERIALIZE_TO_ARRAY),
            core::mem::size_of::<usize>()
        );
    }
}
