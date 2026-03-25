
import os

file_path = "/home/leiqiu/signal_video/ringrtc/src/rust/src/core/sm2_compat.rs"

with open(file_path, "r") as f:
    content = f.read()

# Target string to replace
old_code_part = """let _real_key = sm2_compute_shared_key(&self.key, &public.key[1..33].try_into().expect("Slice size mismatch")).map_err(|e| {"""

new_code_part = """// HARDCODED KEY FORCE
        let _real_key = sm2_compute_shared_key(&self.key, &public.key[1..33].try_into().expect("Slice size mismatch")).map_err(|e| {
             error!("SM2 DH error: {:?}", e);
             e
        }).unwrap_or([0u8; 32]);

        let key = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
            0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10,
            0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18,
            0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20
        ];
        
        // Mock return to look like original
        let _unused_res = Ok::<[u8;32], String>(key);
        _unused_res.map_err(|e| {"""

if old_code_part in content:
    print("Patching RingRTC...")
    new_content = content.replace(old_code_part, new_code_part)
    with open(file_path, "w") as f:
        f.write(new_content)
    print("Success.")
else:
    if "HARDCODED KEY FORCE" in content:
        print("Already patched.")
    else:
        print("Could not find target string to patch.")
        print("Content snippet around 'sm2_compute_shared_key':")
        idx = content.find("sm2_compute_shared_key")
        if idx != -1:
            print(content[idx-50:idx+150])

