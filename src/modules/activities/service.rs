use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;
use validator::Validate;

use super::{
    model::{Activity, ActivityInput, ActivityRow},
    repository,
};
use crate::{
    error::{AppError, AppResult},
    infra::cache::Cache,
    modules::media::MediaService,
    web::pagination::{PageParams, Paginated},
};

const CACHE_TTL: Duration = Duration::from_secs(300);
const CACHE_LIST_KEY: &str = "activities:published";

fn slug_cache_key(slug: &str) -> String {
    format!("activities:slug:{slug}")
}

#[derive(Clone)]
pub struct ActivityService {
    db: PgPool,
    cache: Cache,
    media: MediaService,
}

impl ActivityService {
    pub fn new(db: PgPool, cache: Cache, media: MediaService) -> Self {
        Self { db, cache, media }
    }

    // ---- Public catalogue (read-heavy, cached) ----

    pub async fn list_published(&self) -> AppResult<Vec<Activity>> {
        if let Some(list) = self.cache.get_json(CACHE_LIST_KEY).await {
            return Ok(list);
        }
        let list: Vec<Activity> = repository::list_published(&self.db)
            .await?
            .into_iter()
            .map(|row| self.to_dto(row))
            .collect();
        self.cache.set_json(CACHE_LIST_KEY, &list, CACHE_TTL).await;
        Ok(list)
    }

    pub async fn get_published(&self, slug: &str) -> AppResult<Activity> {
        let key = slug_cache_key(slug);
        if let Some(activity) = self.cache.get_json(&key).await {
            return Ok(activity);
        }
        let activity = repository::find_published_by_slug(&self.db, slug)
            .await?
            .map(|row| self.to_dto(row))
            .ok_or(AppError::NotFound)?;
        self.cache.set_json(&key, &activity, CACHE_TTL).await;
        Ok(activity)
    }

    // ---- Back-office ----

    pub async fn get(&self, id: Uuid) -> AppResult<Activity> {
        repository::find(&self.db, id)
            .await?
            .map(|row| self.to_dto(row))
            .ok_or(AppError::NotFound)
    }

    pub async fn list_all(&self, params: PageParams) -> AppResult<Paginated<Activity>> {
        let (rows, total) = tokio::try_join!(
            repository::list(&self.db, params.limit(), params.offset()),
            repository::count(&self.db),
        )?;
        let data = rows.into_iter().map(|row| self.to_dto(row)).collect();
        Ok(Paginated::new(data, params, total))
    }

    pub async fn create(&self, input: ActivityInput) -> AppResult<Activity> {
        self.validate(&input).await?;
        let row = repository::insert(&self.db, &input).await?;
        self.invalidate(&[&row.slug]).await;
        Ok(self.to_dto(row))
    }

    pub async fn update(&self, id: Uuid, input: ActivityInput) -> AppResult<Activity> {
        self.validate(&input).await?;
        let previous = repository::find(&self.db, id)
            .await?
            .ok_or(AppError::NotFound)?;
        let row = repository::update(&self.db, id, &input)
            .await?
            .ok_or(AppError::NotFound)?;
        self.invalidate(&[&previous.slug, &row.slug]).await;
        Ok(self.to_dto(row))
    }

    /// The cover media stays in the library (it may be reused); delete it from there.
    pub async fn delete(&self, id: Uuid) -> AppResult<()> {
        let slug = repository::delete(&self.db, id)
            .await?
            .ok_or(AppError::NotFound)?;
        self.invalidate(&[&slug]).await;
        Ok(())
    }

    async fn validate(&self, input: &ActivityInput) -> AppResult<()> {
        input.validate()?;
        if let Some(media_id) = input.cover_media_id {
            self.media.ensure_public_image(media_id).await?;
        }
        Ok(())
    }

    async fn invalidate(&self, slugs: &[&str]) {
        let keys = slugs
            .iter()
            .map(|slug| slug_cache_key(slug))
            .chain([CACHE_LIST_KEY.to_string()]);
        self.cache.delete(keys).await;
    }

    fn to_dto(&self, row: ActivityRow) -> Activity {
        Activity {
            cover_image_url: row
                .cover_object_key
                .as_deref()
                .map(|key| self.media.public_url(key)),
            id: row.id,
            slug: row.slug,
            title_fr: row.title_fr,
            title_en: row.title_en,
            description_fr: row.description_fr,
            description_en: row.description_en,
            price_mad: row.price_mad,
            duration_minutes: row.duration_minutes,
            cover_media_id: row.cover_media_id,
            is_published: row.is_published,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}
