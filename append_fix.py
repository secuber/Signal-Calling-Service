import os

missing_code = r"""
        let resolver = CallLinkMemberResolver::from(&root_key);

        for i in 0..2 {
            let peek_response = SerializedPeekInfo {
                era_id: Some("paleozoic".to_string()),
                max_devices: Some(16),
                devices: vec![
                    SerializedPeekDeviceInfo {
                        opaque_user_id: Some(encrypt(uuid_1, &secret_params)),
                        demux_id: 0x11111110,
                    },
                    SerializedPeekDeviceInfo {
                        opaque_user_id: Some(encrypt(uuid_2, &secret_params)),
                        demux_id: 0x22222220,
                    },
                ],
                pending_clients: vec![],
                creator: None,
                call_link_state: None,
            };

            let peek_info = peek_response.deobfuscate(&resolver, Some(root_key.clone()));
            assert_eq!(
                peek_info
                    .devices
                    .iter()
                    .filter_map(|device| device.user_id.as_ref())
                    .collect::<Vec<_>>(),
                vec![uuid_1.as_slice(), uuid_2.as_slice()]
            );
            // The second time around the resolver should use its cache.
            assert_eq!(
                i * 2,
                resolver
                    .cache_hits
                    .load(std::sync::atomic::Ordering::Relaxed)
            );
        }
    }
}
"""

with open("/home/leiqiu/signal_video/ringrtc/src/rust/src/lite/sfu.rs", "a") as f:
    f.write(missing_code)

print("Appended missing code.")
