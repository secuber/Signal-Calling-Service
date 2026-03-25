import hashlib
import hmac
import binascii

def hkdf_extract(salt, input_key_material):
    if salt is None or len(salt) == 0:
        salt = bytes([0] * hashlib.sha256().digest_size)
    prk = hmac.new(salt, input_key_material, hashlib.sha256).digest()
    return prk

def hkdf_expand(prk, info, length):
    t = b""
    okm = b""
    n = 0
    while len(okm) < length:
        n += 1
        t = hmac.new(prk, t + info + bytes([n]), hashlib.sha256).digest()
        okm += t
    return okm[:length]

# Parameters from logs and code
salt = bytes([0] * 32)
ikm = bytes(range(1, 33)) # 0x01 to 0x20
info_str = "Signal_Group_Call_20211105_SignallingDH_SRTPKey_KDF"
info = info_str.encode('ascii') # + nothing else as extra_info is empty

# 1. Extract
prk = hkdf_extract(salt, ikm)

# 2. Expand
# Master Key Material Length = 16 (Client Key) + 12 (Client Salt) + 16 (Server Key) + 12 (Server Salt) = 56 bytes
okm = hkdf_expand(prk, info, 56)

print(f"Calculated OKM: {binascii.hexlify(okm).decode('utf-8')}")

client_key = okm[0:16]
client_salt = okm[16:28]
server_key = okm[28:44]
server_salt = okm[44:56]

print(f"Client Key: {binascii.hexlify(client_key).decode('utf-8')}")
print(f"Client Salt: {binascii.hexlify(client_salt).decode('utf-8')}")
print(f"Server Key: {binascii.hexlify(server_key).decode('utf-8')}")
print(f"Server Salt: {binascii.hexlify(server_salt).decode('utf-8')}")
