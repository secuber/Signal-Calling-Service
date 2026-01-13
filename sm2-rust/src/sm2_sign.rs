use libc::{c_int, c_uchar, size_t};

extern "C" {
    fn sm2_generate_key_raw(private_key: *mut c_uchar, public_key: *mut c_uchar) -> c_int;
    fn sm2_sign_raw(private_key: *const c_uchar, dgst: *const c_uchar, sig: *mut c_uchar, siglen: *mut size_t) -> c_int;
    fn sm2_verify_raw(public_key: *const c_uchar, dgst: *const c_uchar, sig: *const c_uchar, siglen: size_t) -> c_int;
    fn sm2_derive_public_key_raw(private_key: *const u8, public_key: *mut u8) -> c_int;
    fn sm2_derive_shared_secret_raw(private_key: *const u8, public_key: *const u8, shared_key: *mut u8) -> c_int;
}

// 生成 SM2 密钥对
pub fn sm2_generate_key() -> Result<([u8; 32], [u8; 65]), String> {
    let mut private_key = [0u8; 32];
    let mut public_key = [0u8; 65];

    let result = unsafe {
        sm2_generate_key_raw(private_key.as_mut_ptr(), public_key.as_mut_ptr())
    };

    if result == 1 {
        Ok((private_key, public_key))
    } else {
        Err("密钥生成失败".to_string())
    }
}

// 从私钥派生公钥
pub fn sm2_derive_public_key(private_key: &[u8; 32]) -> Result<[u8; 65], String> {
    let mut public_key = [0u8; 65];
    let result = unsafe {
        sm2_derive_public_key_raw(private_key.as_ptr(), public_key.as_mut_ptr())
    };

    if result == 1 {
        Ok(public_key)
    } else {
        Err("从私钥派生公钥失败".to_string())
    }
}

// 计算共享密钥  DH
pub fn sm2_compute_shared_key(private_key: &[u8; 32], public_key: &[u8; 65]) -> Result<[u8; 64], String> {
    let mut shared_key = [0u8; 64];
    let result = unsafe {
        sm2_derive_shared_secret_raw(private_key.as_ptr(), public_key.as_ptr(), shared_key.as_mut_ptr())
    };

    if result == 1 {
        Ok(shared_key)
    } else {
        Err("DH 共享密钥计算失败".to_string())
    }
}

// 签名函数
pub fn sm2_sign(private_key: &[u8; 32], message_digest: &[u8]) -> Result<Vec<u8>, String> {
    let mut sig = vec![0u8; 64];
    let mut siglen: size_t = 0;

    let result = unsafe {
        sm2_sign_raw(
            private_key.as_ptr(),
            message_digest.as_ptr(),
            sig.as_mut_ptr(),
            &mut siglen,
        )
    };

    if result == 1 {
        sig.truncate(siglen);
        Ok(sig)
    } else {
        Err("签名失败".to_string())
    }
}


// 验签函数
pub fn sm2_verify(public_key: &[u8; 65], message_digest: &[u8], signature: &[u8]) -> bool{
    let result = unsafe {
        sm2_verify_raw(
            public_key.as_ptr(),
            message_digest.as_ptr(),
            signature.as_ptr(),
            signature.len(),
        )
    };

    if result == 1 {
        true
    } else {
        // Err("验签失败".to_string())
        false
    }
}