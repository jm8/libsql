pub struct SyncContext {
    sync_url: String,
    auth_token: Option<String>,
    durable_frame_num: u32,
}

impl SyncContext {
    pub fn new(sync_url: String, auth_token: Option<String>) -> Self {
        Self {
            sync_url,
            auth_token,
            durable_frame_num: 0,
        }
    }

    pub fn durable_frame_num(&self) -> u32 {
        self.durable_frame_num
    }

    pub fn sync_url(&self) -> &str {
        &self.sync_url
    }

    pub fn auth_token(&self) -> Option<&str> {
        self.auth_token.as_deref()
    }
}
