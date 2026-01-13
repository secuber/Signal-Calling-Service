#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <gmssl/error.h>
#include <gmssl/mem.h>
#include <gmssl/ghash.h>
#include <gmssl/sm2.h>


// FFI 函数：生成 SM2 密钥对
int sm2_generate_key_raw(uint8_t *private_key, uint8_t *public_key) {
    SM2_KEY sm2_key;
    if (sm2_key_generate(&sm2_key) != 1) {
        error_print();
        return -1;
    }

    memcpy(private_key, sm2_key.private_key, 32);
    sm2_z256_point_to_uncompressed_octets(&sm2_key.public_key, public_key);
    return 1;
}

// 从私钥推导出公钥 (65字节, 0x04 + X + Y)
int sm2_derive_public_key_raw(uint8_t *private_key_bytes, uint8_t *public_key) {
    SM2_KEY sm2_key;
    sm2_z256_t private_key;

    // 小端
    for (int i = 0; i < 4; i++) {
        private_key[i] =
            ((uint64_t)private_key_bytes[i * 8 + 7] << 56) |
            ((uint64_t)private_key_bytes[i * 8 + 6] << 48) |
            ((uint64_t)private_key_bytes[i * 8 + 5] << 40) |
            ((uint64_t)private_key_bytes[i * 8 + 4] << 32) |
            ((uint64_t)private_key_bytes[i * 8 + 3] << 24) |
            ((uint64_t)private_key_bytes[i * 8 + 2] << 16) |
            ((uint64_t)private_key_bytes[i * 8 + 1] << 8)  |
            ((uint64_t)private_key_bytes[i * 8 + 0]);
    }
    

    if (sm2_key_set_private_key(&sm2_key, private_key) != 1) {
        error_print();
        return -1;
    }

    sm2_z256_point_to_uncompressed_octets(&sm2_key.public_key, public_key);
    return 1;
}



// FFI 函数：SM2 密钥交换（计算共享密钥）  DH
int sm2_derive_shared_secret_raw(uint8_t *private_key_bytes, uint8_t *their_public_key, uint8_t *shared_secret) {
    SM2_KEY sm2_pri_key;
    sm2_z256_t private_key;
    // 小端
    for (int i = 0; i < 4; i++) {
        private_key[i] =
            ((uint64_t)private_key_bytes[i * 8 + 7] << 56) |
            ((uint64_t)private_key_bytes[i * 8 + 6] << 48) |
            ((uint64_t)private_key_bytes[i * 8 + 5] << 40) |
            ((uint64_t)private_key_bytes[i * 8 + 4] << 32) |
            ((uint64_t)private_key_bytes[i * 8 + 3] << 24) |
            ((uint64_t)private_key_bytes[i * 8 + 2] << 16) |
            ((uint64_t)private_key_bytes[i * 8 + 1] << 8)  |
            ((uint64_t)private_key_bytes[i * 8 + 0]);
    }
    // 初始化私钥
    if (sm2_key_set_private_key(&sm2_pri_key, private_key) != 1) {
        error_print();
        return -1;
    }

    if (sm2_ecdh(&sm2_pri_key, their_public_key, 65, shared_secret) != 1) {
        error_print();
        return -1;
    }

    return 1;
}



int sm2_sign_raw(const uint8_t *private_key, const uint8_t *dgst, uint8_t *sig, size_t *siglen) {
    SM2_KEY sm2_key;
    memcpy(sm2_key.private_key, private_key, 32);

    SM2_SIGNATURE signature;
    if (sm2_do_sign(&sm2_key, dgst, &signature) != 1) {
        error_print();
        return -1;
    }

    memcpy(sig, signature.r, 32);
    memcpy(sig + 32, signature.s, 32);
    *siglen = 64;
    return 1;
}

int sm2_verify_raw(const uint8_t *public_key, const uint8_t *dgst, const uint8_t *sig, size_t siglen) {
    if (siglen != 64) {
        error_print();
        return -1;
    }

    SM2_KEY sm2_key;
    if (sm2_z256_point_from_octets(&sm2_key.public_key, public_key, 65) != 1) {
        error_print();
        return -1;
    }

    SM2_SIGNATURE signature;
    memcpy(signature.r, sig, 32);
    memcpy(signature.s, sig + 32, 32);

    if (sm2_do_verify(&sm2_key, dgst, &signature) != 1) {
        error_print();
        return -1;
    }
    return 1;
}
