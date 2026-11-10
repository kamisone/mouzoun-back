use serde::{Deserialize, Serialize};

pub const MAX_PER_PAGE: i64 = 100;

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct PageParams {
    #[serde(default = "default_page")]
    pub page: i64,
    #[serde(default = "default_per_page")]
    pub per_page: i64,
}

impl PageParams {
    pub fn new(page: i64, per_page: i64) -> Self {
        Self { page, per_page }
    }
    pub fn page(&self) -> i64 {
        self.page.max(1)
    }
    pub fn limit(&self) -> i64 {
        self.per_page.clamp(1, MAX_PER_PAGE)
    }
    pub fn offset(&self) -> i64 {
        (self.page() - 1) * self.limit()
    }
}

impl Default for PageParams {
    fn default() -> Self {
        Self::new(default_page(), default_per_page())
    }
}

fn default_page() -> i64 {
    1
}
fn default_per_page() -> i64 {
    20
}

#[derive(Debug, Serialize)]
pub struct Paginated<T> {
    pub data: Vec<T>,
    pub page: i64,
    pub per_page: i64,
    pub total: i64,
}

impl<T> Paginated<T> {
    pub fn new(data: Vec<T>, params: PageParams, total: i64) -> Self {
        Self {
            data,
            page: params.page(),
            per_page: params.limit(),
            total,
        }
    }
}
