use async_trait::async_trait;
use aws_sdk_s3::error::{HeadBucketError, HeadBucketErrorKind};
use aws_sdk_s3::model::{CommonPrefix, Delete, ObjectIdentifier};
use aws_sdk_s3::types::{ByteStream, SdkError};
use aws_sdk_s3::Error as AwsSdkError;
use aws_sdk_s3::RetryConfig;
use aws_sdk_s3::{Client, Credentials, Endpoint, Region};
use aws_smithy_async::rt::sleep::default_async_sleep;
use bytes::Bytes;
use clap::builder::ArgPredicate;
use datafusion::arrow::datatypes::Schema;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{
    ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl,
};
use datafusion::datasource::object_store::ObjectStoreRegistry;
use datafusion::execution::runtime_env::{RuntimeConfig, RuntimeEnv};
use datafusion::prelude::{SessionConfig, SessionContext};
use futures::StreamExt;
use http::Uri;
use object_store::aws::AmazonS3Builder;
use object_store::limit::LimitStore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::iter::Iterator;
use std::sync::Arc;

use crate::alerts::Alerts;
use crate::option::{StorageOpt, CONFIG};
use crate::query::Query;
use crate::stats::Stats;
use crate::storage::{LogStream, ObjectStorage, ObjectStorageError};

// Default object storage currently is DO Spaces bucket
// Any user who starts the Parseable server with default configuration
// will point to this bucket and will see any data present on this bucket
const DEFAULT_S3_URL: &str = "https://minio.parseable.io:9000";
const DEFAULT_S3_REGION: &str = "us-east-1";
const DEFAULT_S3_BUCKET: &str = "parseable";
const DEFAULT_S3_ACCESS_KEY: &str = "minioadmin";
const DEFAULT_S3_SECRET_KEY: &str = "minioadmin";

// max concurrent request allowed for datafusion object store
const MAX_OBJECT_STORE_REQUESTS: usize = 1000;

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectStoreFormat {
    #[serde(rename = "objectstore-format")]
    pub version: String,
}

impl ObjectStoreFormat {
    pub fn new() -> Self {
        Self {
            version: "v1".to_string(),
        }
    }
}

lazy_static::lazy_static! {
    #[derive(Debug)]
    pub static ref S3_CONFIG: Arc<S3Config> = Arc::new(CONFIG.storage().clone());

    // runtime to be used in query session
    pub static ref STORAGE_RUNTIME: Arc<RuntimeEnv> = {

        let s3 = AmazonS3Builder::new()
            .with_region(&S3_CONFIG.s3_region)
            .with_endpoint(&S3_CONFIG.s3_endpoint_url)
            .with_bucket_name(&S3_CONFIG.s3_bucket_name)
            .with_access_key_id(&S3_CONFIG.s3_access_key_id)
            .with_secret_access_key(&S3_CONFIG.s3_secret_key)
            // allow http for local instances
            .with_allow_http(true)
            .build()
            .unwrap();

        // limit objectstore to a concurrent request limit
        let s3 = LimitStore::new(s3, MAX_OBJECT_STORE_REQUESTS);

        let object_store_registry = ObjectStoreRegistry::new();
        object_store_registry.register_store("s3", &S3_CONFIG.s3_bucket_name, Arc::new(s3));

        let config = RuntimeConfig::new().with_object_store_registry(Arc::new(object_store_registry));

        let runtime = RuntimeEnv::new(config).unwrap();

        Arc::new(runtime)

    };
}

#[derive(Debug, Clone, clap::Args)]
#[command(name = "S3 config", about = "configuration for AWS S3 SDK")]
pub struct S3Config {
    /// The endpoint to AWS S3 or compatible object storage platform
    #[arg(
        long,
        env = "P_S3_URL",
        value_name = "url",
        default_value_if("demo", ArgPredicate::IsPresent, DEFAULT_S3_URL)
    )]
    pub s3_endpoint_url: String,

    /// The access key for AWS S3 or compatible object storage platform
    #[arg(
        long,
        env = "P_S3_ACCESS_KEY",
        value_name = "access-key",
        default_value_if("demo", ArgPredicate::IsPresent, DEFAULT_S3_ACCESS_KEY)
    )]
    pub s3_access_key_id: String,

    /// The secret key for AWS S3 or compatible object storage platform
    #[arg(
        long,
        env = "P_S3_SECRET_KEY",
        value_name = "secret-key",
        default_value_if("demo", ArgPredicate::IsPresent, DEFAULT_S3_SECRET_KEY)
    )]
    pub s3_secret_key: String,

    /// The region for AWS S3 or compatible object storage platform
    #[arg(
        long,
        env = "P_S3_REGION",
        value_name = "region",
        default_value_if("demo", ArgPredicate::IsPresent, DEFAULT_S3_REGION)
    )]
    pub s3_region: String,

    /// The AWS S3 or compatible object storage bucket to be used for storage
    #[arg(
        long,
        env = "P_S3_BUCKET",
        value_name = "bucket-name",
        default_value_if("demo", ArgPredicate::IsPresent, DEFAULT_S3_BUCKET)
    )]
    pub s3_bucket_name: String,
}

impl StorageOpt for S3Config {
    fn bucket_name(&self) -> &str {
        &self.s3_bucket_name
    }

    fn endpoint_url(&self) -> &str {
        &self.s3_endpoint_url
    }
}

struct S3Options {
    endpoint: Endpoint,
    region: Region,
    creds: Credentials,
}

impl S3Options {
    fn new() -> Self {
        let uri = S3_CONFIG.s3_endpoint_url.parse::<Uri>().unwrap();
        let endpoint = Endpoint::immutable(uri);
        let region = Region::new(&S3_CONFIG.s3_region);
        let creds = Credentials::new(
            &S3_CONFIG.s3_access_key_id,
            &S3_CONFIG.s3_secret_key,
            None,
            None,
            "",
        );

        Self {
            endpoint,
            region,
            creds,
        }
    }
}

pub struct S3 {
    client: aws_sdk_s3::Client,
}

impl S3 {
    pub fn new() -> Self {
        let options = S3Options::new();
        let config = aws_sdk_s3::Config::builder()
            .region(options.region)
            .endpoint_resolver(options.endpoint)
            .credentials_provider(options.creds)
            .retry_config(RetryConfig::standard().with_max_attempts(5))
            .sleep_impl(default_async_sleep().expect("sleep impl is provided for tokio rt"))
            .build();

        let client = Client::from_conf(config);

        Self { client }
    }

    async fn _put_schema(&self, stream_name: String, body: String) -> Result<(), AwsSdkError> {
        let _resp = self
            .client
            .put_object()
            .bucket(&S3_CONFIG.s3_bucket_name)
            .key(format!("{}/.schema", stream_name))
            .body(body.into_bytes().into())
            .send()
            .await?;

        Ok(())
    }

    async fn _create_stream(&self, stream_name: &str, format: Vec<u8>) -> Result<(), AwsSdkError> {
        // create ./schema empty file in the stream-name prefix
        // this indicates that the stream has been created
        // but doesn't have any content yet
        let _resp = self
            .client
            .put_object()
            .bucket(&S3_CONFIG.s3_bucket_name)
            .key(format!("{}/.schema", stream_name))
            .send()
            .await?;
        self._put_parseable_config(stream_name, format).await?;
        // Prefix created on S3, now create the directory in
        // the local storage as well
        let _res = fs::create_dir_all(CONFIG.parseable.local_stream_data_path(stream_name));
        Ok(())
    }

    async fn _put_parseable_config(
        &self,
        stream_name: &str,
        body: Vec<u8>,
    ) -> Result<(), AwsSdkError> {
        let _resp = self
            .client
            .put_object()
            .bucket(&S3_CONFIG.s3_bucket_name)
            .key(format!("{}/.parseable.json", stream_name))
            .body(body.into())
            .send()
            .await?;

        Ok(())
    }

    async fn _delete_stream(&self, stream_name: &str) -> Result<(), AwsSdkError> {
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(&S3_CONFIG.s3_bucket_name)
            .prefix(format!("{}/", stream_name))
            .into_paginator()
            .send();

        let mut delete_objects: Vec<ObjectIdentifier> = vec![];
        while let Some(page) = pages.next().await {
            let page = page?;
            for obj in page.contents.unwrap() {
                let obj_id = ObjectIdentifier::builder().set_key(obj.key).build();
                delete_objects.push(obj_id);
            }
        }

        let delete = Delete::builder().set_objects(Some(delete_objects)).build();

        self.client
            .delete_objects()
            .bucket(&S3_CONFIG.s3_bucket_name)
            .delete(delete)
            .send()
            .await?;

        Ok(())
    }

    async fn _put_alerts(&self, stream_name: &str, body: Vec<u8>) -> Result<(), AwsSdkError> {
        let _resp = self
            .client
            .put_object()
            .bucket(&S3_CONFIG.s3_bucket_name)
            .key(format!("{}/.alert.json", stream_name))
            .body(body.into())
            .send()
            .await?;

        Ok(())
    }

    async fn _get_schema(&self, stream_name: &str) -> Result<Bytes, AwsSdkError> {
        self._get(stream_name, "schema").await
    }

    async fn _alert_exists(&self, stream_name: &str) -> Result<Bytes, AwsSdkError> {
        self._get(stream_name, "alert.json").await
    }

    async fn _get_parseable_config(&self, stream_name: &str) -> Result<Bytes, AwsSdkError> {
        self._get(stream_name, "parseable.json").await
    }

    async fn _get(&self, stream_name: &str, resource: &str) -> Result<Bytes, AwsSdkError> {
        let resp = self
            .client
            .get_object()
            .bucket(&S3_CONFIG.s3_bucket_name)
            .key(format!("{}/.{}", stream_name, resource))
            .send()
            .await?;
        let body = resp.body.collect().await;
        let body_bytes = body.unwrap().into_bytes();
        Ok(body_bytes)
    }

    #[allow(dead_code)]
    async fn prefix_exists(&self, prefix: &str) -> Result<bool, AwsSdkError> {
        // TODO check if head object is faster compared to list objects
        let resp = self
            .client
            .list_objects_v2()
            .bucket(&S3_CONFIG.s3_bucket_name)
            .prefix(prefix)
            .max_keys(1)
            .send()
            .await?;

        let result = resp.contents.is_some();

        Ok(result)
    }

    async fn _list_streams(&self) -> Result<Vec<LogStream>, AwsSdkError> {
        let resp = self
            .client
            .list_objects_v2()
            .bucket(&S3_CONFIG.s3_bucket_name)
            .delimiter('/')
            .send()
            .await?;

        let common_prefixes = resp.common_prefixes().unwrap_or_default();

        // return prefixes at the root level
        let logstreams: Vec<_> = common_prefixes
            .iter()
            .filter_map(CommonPrefix::prefix)
            .filter_map(|name| name.strip_suffix('/'))
            .map(String::from)
            .map(|name| LogStream { name })
            .collect();

        Ok(logstreams)
    }

    async fn _upload_file(&self, key: &str, path: &str) -> Result<(), AwsSdkError> {
        let body = ByteStream::from_path(path).await.unwrap();
        let resp = self
            .client
            .put_object()
            .bucket(&S3_CONFIG.s3_bucket_name)
            .key(key)
            .body(body)
            .send()
            .await?;
        log::trace!("{:?}", resp);

        Ok(())
    }
}

#[async_trait]
impl ObjectStorage for S3 {
    fn new() -> Self {
        Self::new()
    }

    async fn check(&self) -> Result<(), ObjectStorageError> {
        self.client
            .head_bucket()
            .bucket(&S3_CONFIG.s3_bucket_name)
            .send()
            .await
            .map(|_| ())
            .map_err(|err| err.into())
    }

    async fn put_schema(
        &self,
        stream_name: String,
        schema: &Schema,
    ) -> Result<(), ObjectStorageError> {
        self._put_schema(stream_name, serde_json::to_string(&schema)?)
            .await?;

        Ok(())
    }

    async fn create_stream(&self, stream_name: &str) -> Result<(), ObjectStorageError> {
        let format = ObjectStoreFormat::new();
        let body = serde_json::to_vec(&format)?;
        self._create_stream(stream_name, body).await?;

        Ok(())
    }

    async fn delete_stream(&self, stream_name: &str) -> Result<(), ObjectStorageError> {
        self._delete_stream(stream_name).await?;

        Ok(())
    }

    async fn put_alerts(
        &self,
        stream_name: &str,
        alerts: &Alerts,
    ) -> Result<(), ObjectStorageError> {
        let body = serde_json::to_vec(alerts)?;
        self._put_alerts(stream_name, body).await?;

        Ok(())
    }

    async fn get_schema(&self, stream_name: &str) -> Result<Option<Schema>, ObjectStorageError> {
        let body_bytes = self._get_schema(stream_name).await?;
        let schema = serde_json::from_slice(&body_bytes).ok();
        Ok(schema)
    }

    async fn get_alerts(&self, stream_name: &str) -> Result<Alerts, ObjectStorageError> {
        let res = self._alert_exists(stream_name).await;

        match res {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_default()),
            Err(e) => match e {
                AwsSdkError::NoSuchKey(_) => Ok(Alerts::default()),
                e => Err(e.into()),
            },
        }
    }

    async fn get_stats(&self, stream_name: &str) -> Result<Stats, ObjectStorageError> {
        let parseable_metadata = self._get_parseable_config(stream_name).await?;
        let parseable_metadata: Value =
            serde_json::from_slice(&parseable_metadata).expect("parseable config is valid json");

        let stats = &parseable_metadata["stats"];

        let stats = serde_json::from_value(stats.clone()).unwrap_or_default();

        Ok(stats)
    }

    async fn put_stats(&self, stream_name: &str, stats: &Stats) -> Result<(), ObjectStorageError> {
        let stats = serde_json::to_value(stats).expect("stats are perfectly serializable");
        let parseable_metadata = self._get_parseable_config(stream_name).await?;
        let mut parseable_metadata: Value =
            serde_json::from_slice(&parseable_metadata).expect("parseable config is valid json");

        parseable_metadata["stats"] = stats;

        self._put_parseable_config(stream_name, parseable_metadata.to_string().into_bytes())
            .await?;
        Ok(())
    }

    async fn list_streams(&self) -> Result<Vec<LogStream>, ObjectStorageError> {
        let streams = self._list_streams().await?;

        Ok(streams)
    }

    async fn upload_file(&self, key: &str, path: &str) -> Result<(), ObjectStorageError> {
        self._upload_file(key, path).await?;

        Ok(())
    }

    async fn query(
        &self,
        query: &Query,
        results: &mut Vec<RecordBatch>,
    ) -> Result<(), ObjectStorageError> {
        let ctx =
            SessionContext::with_config_rt(SessionConfig::default(), Arc::clone(&STORAGE_RUNTIME));

        // Get all prefix paths and convert them into futures which yeilds ListingTableUrl
        let prefixes = query
            .get_prefixes()
            .into_iter()
            .map(|prefix| {
                let path = format!("s3://{}/{}", &S3_CONFIG.s3_bucket_name, prefix);
                ListingTableUrl::parse(path).unwrap()
            })
            .collect();

        let file_format = ParquetFormat::default().with_enable_pruning(true);
        let listing_options = ListingOptions {
            file_extension: ".data.parquet".to_string(),
            format: Arc::new(file_format),
            table_partition_cols: vec![],
            collect_stat: true,
            target_partitions: 1,
        };

        let config = ListingTableConfig::new_with_multi_paths(prefixes)
            .with_listing_options(listing_options)
            .with_schema(Arc::clone(&query.schema));

        let table = ListingTable::try_new(config)?;
        ctx.register_table(query.stream_name.as_str(), Arc::new(table))?;

        // execute the query and collect results
        let df = ctx.sql(&query.query).await?;
        results.extend(df.collect().await?);

        Ok(())
    }
}

impl From<AwsSdkError> for ObjectStorageError {
    fn from(error: AwsSdkError) -> Self {
        ObjectStorageError::UnhandledError(Box::new(error))
    }
}

impl From<SdkError<HeadBucketError>> for ObjectStorageError {
    fn from(error: SdkError<HeadBucketError>) -> Self {
        match error {
            SdkError::ServiceError {
                err:
                    HeadBucketError {
                        kind: HeadBucketErrorKind::NotFound(_),
                        ..
                    },
                ..
            } => ObjectStorageError::NoSuchBucket(S3_CONFIG.bucket_name().to_string()),
            SdkError::ServiceError {
                err:
                    HeadBucketError {
                        kind: HeadBucketErrorKind::Unhandled(err),
                        ..
                    },
                ..
            } => ObjectStorageError::AuthenticationError(err),
            SdkError::DispatchFailure(err) => ObjectStorageError::ConnectionError(Box::new(err)),
            SdkError::TimeoutError(err) => ObjectStorageError::ConnectionError(err),
            err => ObjectStorageError::UnhandledError(Box::new(err)),
        }
    }
}

impl From<serde_json::Error> for ObjectStorageError {
    fn from(error: serde_json::Error) -> Self {
        ObjectStorageError::UnhandledError(Box::new(error))
    }
}
