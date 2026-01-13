use libc::{c_int, c_uchar};

//定义 SM2 密钥对的长度
pub const SM2_PRIVATE_KEY_LEN: usize = 32;
pub const SM2_PUBLIC_KEY_LEN: usize = 64;

//声明 C 层 FFI 函数
extern "C" {
    pub fn sm2_generate_key_raw(
        private_key: *mut c_uchar,
        public_key: *mut c_uchar,
    ) -> c_int;
}


// 生成 SM2 密钥对
pub fn sm2_generate_key() -> Result<([u8; SM2_PRIVATE_KEY_LEN], [u8; SM2_PUBLIC_KEY_LEN]), String> {
    let mut private_key = [0u8; SM2_PRIVATE_KEY_LEN]; // 分配 32 字节的私钥缓冲区
    let mut public_key = [0u8; SM2_PUBLIC_KEY_LEN];   // 分配 64 字节的公钥缓冲区

    // 调用 FFI 函数
    let result = unsafe { sm2_generate_key_raw(private_key.as_mut_ptr(), public_key.as_mut_ptr()) };

    // 判断结果
    if result == 1 {
        Ok((private_key, public_key)) // 返回生成的密钥对
    } else {
        Err("SM2 key generation failed".to_string()) // 返回错误信息
    }
}
