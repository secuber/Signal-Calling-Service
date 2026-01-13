// server x86的
// fn main() {
//     println!("cargo:rerun-if-changed=src/wrapper.c");

//     let gmssl_lib = "/home/leiqiu/GmSSL/build/lib";  // GmSSL 静态库路径
//     let gmssl_include = "/home/leiqiu/GmSSL/include";  // GmSSL 头文件路径

//     cc::Build::new()
//         .file("src/wrapper.c")
//         .include(gmssl_include)
//         .flag("-fPIC")  // <-- 编译libsignal-service的时候，报错的，添加此项
//         .compile("wrapper");

//     println!("cargo:rustc-link-lib=static=gmssl");  // 使用静态链接，如果是动态则改为 dynamic
//     println!("cargo:rustc-link-search=native={}", gmssl_lib);
// }



// android arm64的
fn main() {
    println!("cargo:rerun-if-changed=src/wrapper.c");

    // Use environment variables set by build_jni.sh, or default to local paths
    let gmssl_lib = std::env::var("GMSSL_LIB_DIR")
        .unwrap_or_else(|_| "/home/leiqiu/GmSSL/build/lib".to_string());
    let gmssl_include = std::env::var("GMSSL_INCLUDE_DIR")
        .unwrap_or_else(|_| "/home/leiqiu/GmSSL/include".to_string());

    let mut build = cc::Build::new();
    build.file("src/wrapper.c")
         .include(&gmssl_include)
         .flag("-fPIC");

    // When cross-compiling for Android, the CC and CFLAGS environment variables 
    // set in build_jni.sh will be automatically picked up by the cc crate.
    
    build.compile("wrapper");

    println!("cargo:rustc-link-lib=static=gmssl");
    println!("cargo:rustc-link-search=native={}", gmssl_lib);
}

