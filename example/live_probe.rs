//! 真机探针:加载**部署目录**的 librime_next.dll + 真实用户数据,
//! 打 "h" 后用扩展 data_size 调 get_context,看 total_candidates 出不出来。
use std::ffi::CString;

#[allow(non_snake_case)]
mod raw {
    // 只声明用得到的薄绑定(与 Engine/bindgen 同布局)
    #[repr(C)] pub struct RimeComposition { pub length: i32, pub cursor_pos: i32, pub sel_start: i32, pub sel_end: i32, pub preedit: *mut std::ffi::c_char, }
    #[repr(C)] pub struct RimeCandidate { pub text: *mut std::ffi::c_char, pub comment: *mut std::ffi::c_char, #[allow(dead_code)] pub r#type: *mut std::ffi::c_char, }
    #[repr(C)] pub struct RimeMenu { pub page_size: i32, pub page_no: i32, pub is_last_page: i32, pub highlighted_candidate_index: i32, pub num_candidates: i32, pub candidates: *mut RimeCandidate, pub select_keys: *mut std::ffi::c_char, }
    #[repr(C)] pub struct RimeContext { pub data_size: i32, pub composition: RimeComposition, pub menu: RimeMenu, pub commit_text_preview: *mut std::ffi::c_char, pub select_labels: *mut *mut std::ffi::c_char, pub predictions: *mut *mut std::ffi::c_char, pub predictions_len: i32, pub total_candidates: i32, }
    #[repr(C)] pub struct RimeSessionId(pub usize);
}

#[link(name = "librime_next")]
unsafe extern "C" {
    fn rime_initialize(traits: *const RimeTraits) -> i32;
    fn rime_create_session() -> usize;
    fn rime_process_key(id: usize, key: i32, mask: i32) -> i32;
    fn rime_get_context(id: usize, ctx: *mut raw::RimeContext) -> i32;
    fn rime_free_context(ctx: *mut raw::RimeContext) -> i32;
}

#[repr(C)]
pub struct RimeTraits {
    pub int_size: usize, pub bool_size: usize, pub ptr_size: usize, pub dbl_size: usize,
    pub data_size: usize,
    pub app_name: *const std::ffi::c_char,
    pub distribution_name: *const std::ffi::c_char,
    pub distribution_code_name: *const std::ffi::c_char,
    pub distribution_version: *const std::ffi::c_char,
    pub modules: *const *const std::ffi::c_char,
    pub min_log_level: i32, pub log_dir: *const std::ffi::c_char,
    pub prebuilt_data_dir: *const std::ffi::c_char, pub staging_dir: *const std::ffi::c_char,
}

fn main() {
    unsafe { std::env::set_var("RIMEutzer", "1"); }
    let user = std::env::var("USERPROFILE").unwrap();
    let shared = format!(r"{user}\AppData\Roaming\Rime-Next");
    let local = format!(r"{user}\AppData\Local\Rime-Next");
    let c = |s: String| CString::new(s).unwrap().into_raw();
    let app = c("rime-weasel".into());
    let dist = c("Weasel-Next".into());
    let dist_code = c("Weasel".into());
    let dist_ver = c("0.18.7".into());
    let shared_p = c(shared.clone());
    let user_p = c(shared.clone());
    let log_p = c(format!(r"{local}\Logs"));
    let staging = c(format!(r"{shared}\build"));
    let traits = RimeTraits {
        int_size: 4, bool_size: 1, ptr_size: 8, dbl_size: 8, data_size: std::mem::size_of::<usize>() as usize,
        app_name: app, distribution_name: dist, distribution_code_name: dist_code, distribution_version: dist_ver,
        modules: std::ptr::null(), min_log_level: 0, log_dir: log_p, prebuilt_data_dir: shared_p, staging_dir: staging,
    };
    unsafe {
        rime_initialize(&traits);
        let id = rime_create_session();
        println!("session id = {id}");
        if id == 0 { println!("✗ 建会话失败"); return; }
        for ch in "h".chars() {
            let eaten = rime_process_key(id, ch as i32, 0);
            println!("key {ch} eaten={eaten}");
        }
        let mut ctx: raw::RimeContext = std::mem::zeroed();
        ctx.data_size = (std::mem::size_of::<raw::RimeContext>() - std::mem::size_of::<i32>()) as i32;
        let ok = rime_get_context(id, &mut ctx);
        println!("get_context ok={ok}");
        println!("menu.num_candidates={} page_size={} page_no={} is_last_page={}",
            ctx.menu.num_candidates, ctx.menu.page_size, ctx.menu.page_no, ctx.menu.is_last_page);
        println!("total_candidates={} (扩展字段)", ctx.total_candidates);
        println!("predictions_len={}", ctx.predictions_len);
        rime_free_context(&mut ctx);
    }
}
