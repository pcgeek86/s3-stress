use aws_config::Region;
use aws_sdk_account as acct;
use colorize::AnsiColor;
use std::{str::FromStr, sync::Arc};

use aws_runtime::env_config::file::Builder;
use aws_types::SdkConfig;
use aws_sdk_s3::{self as s3, operation::list_objects_v2::ListObjectsV2Output, primitives::{ByteStream, SdkBody}, types::{builders::CreateBucketConfigurationBuilder, BucketLocationConstraint}, Client};
use futures::future::join_all;
use inquire::{validator::Validation, CustomUserError};

#[tokio::main]
async fn main() {
    operation_select().await;
}

// This trait makes it easier to get an Arc<T> from various types
trait AsArc {
    fn as_arc(self) -> Arc<Self> where Self: Sized {
        Arc::new(self)
    }
}

impl AsArc for Client {}
impl AsArc for acct::Client {}
impl AsArc for String {}
impl AsArc for Vec<String> {}
impl AsArc for SdkConfig {}

// Main entry point of the application. Select a bucket and operation to perform.
async fn operation_select() {
    let aws_cfg = select_authentication().await.as_arc();

    // Create AWS service clients
    let acct_client_arc = acct::Client::new(&aws_cfg).as_arc();
    let s3_client_arc = s3::Client::new(&aws_cfg).as_arc();

    let region_list = get_aws_regions(acct_client_arc.clone()).await.as_arc();

    loop {
        let operation_list = vec!["Cleanup bucket", "Create objects", "Create bucket", "Delete bucket"];
        let selected_operation = inquire::Select::new("Select an operation", operation_list).prompt().unwrap();
    
        match selected_operation {
            "Cleanup bucket" => {
                let bucket_name = select_bucket(s3_client_arc.clone()).await;
                let s3_client = get_s3_client_for_bucket(s3_client_arc.clone(), aws_cfg.clone(), &bucket_name).await;
                operation_cleanup_bucket(s3_client, bucket_name.into()).await;
            }
            "Create objects" => {
                let bucket_name = select_bucket(s3_client_arc.clone()).await;
                let s3_client = get_s3_client_for_bucket(s3_client_arc.clone(), aws_cfg.clone(), &bucket_name).await;
                operation_create_objects(s3_client, bucket_name.into()).await;
            }
            "Create bucket" => {
                operation_create_bucket(aws_cfg.clone(), region_list.clone()).await;
            }
            "Delete bucket" => {
                let bucket_name = select_bucket(s3_client_arc.clone()).await;
                let s3_client = get_s3_client_for_bucket(s3_client_arc.clone(), aws_cfg.clone(), &bucket_name).await;
                delete_bucket(s3_client, &bucket_name).await;
            }
            "q" | "quit" | "exit" => { std::process::exit(0) }
            _ => { }
        }
    }
}

async fn select_bucket(s3_client: Arc<Client>) -> String {
    let bucket_list = s3_client.clone().list_buckets().send().await.unwrap().buckets.unwrap();
    let bucket_list = bucket_list.iter().map(|i| i.name.as_ref().unwrap()).collect();
    inquire::Select::new("Please select an S3 bucket", bucket_list).prompt().unwrap().to_owned()
}

async fn get_s3_client_for_bucket(s3_client: Arc<Client>, aws_cfg: Arc<SdkConfig>, bucket_name: &String) -> s3::Client {
    let bucket_location = s3_client.get_bucket_location()
        .bucket(bucket_name).send().await.unwrap().location_constraint.unwrap().to_string();
    println!("Bucket location: {0}", bucket_location.clone().green());

    let bucket_region = Region::new(bucket_location);
    let new_aws_cfg = aws_cfg.as_ref().clone().into_builder()
        .region(bucket_region).build();
    s3::Client::new(&new_aws_cfg)
}

async fn delete_bucket(s3_client: Client, bucket_name: &String) {
    let delete_result = s3_client.delete_bucket()
        .bucket(bucket_name)
        .send().await;

    if delete_result.is_err() {
        let sdk_err = delete_result.err().unwrap().into_service_error();
        let svc_err = sdk_err.meta().message();
        let message = svc_err.unwrap().to_string();
        println!("{0}", message.red());
    }
}

// Query a list of available AWS regions
async fn get_aws_regions(acct_client: Arc<acct::Client>) -> Vec<String> {
    acct_client.list_regions()
        .region_opt_status_contains(aws_sdk_account::types::RegionOptStatus::EnabledByDefault)
        .region_opt_status_contains(aws_sdk_account::types::RegionOptStatus::Enabled)
        .region_opt_status_contains(aws_sdk_account::types::RegionOptStatus::Enabling)
        .send().await
        .unwrap().regions.unwrap()
        .iter().map(|r| r.region_name.clone().unwrap()).collect()
}

async fn operation_create_bucket(aws_cfg: Arc<SdkConfig>, region_list: Arc<Vec<String>>) {

    let new_bucket_name = inquire::Text::new("🪣 Enter new bucket name")
        .with_default(uuid::Uuid::new_v4().to_string().as_str())
        .prompt().unwrap();

    let new_bucket_location = inquire::Select::new("New bucket location", region_list.to_vec()).prompt().unwrap();

    let new_aws_cfg = aws_cfg.as_ref().clone().into_builder()
        .region(Region::new(new_bucket_location.clone()))
        .build();
    let s3_client = s3::Client::new(&new_aws_cfg);

    let location = BucketLocationConstraint::from_str(new_bucket_location.as_str()).unwrap();
    let cbc = CreateBucketConfigurationBuilder::default()
        .location_constraint(location)
        .build();

    let create_result = s3_client.create_bucket()
        .bucket(new_bucket_name)
        .create_bucket_configuration(cbc)
        .send().await;

    if create_result.is_err() {
        println!("{0:?}", create_result.err());
    }
}

async fn operation_create_objects(s3_client: Client, bucket_name: String) {
    let object_count: u32 = inquire::Text::new("How many objects should I create?")
        .with_validator(validate_number)
        .prompt().unwrap().parse().unwrap();

    let mut join_handle_list = vec![];
    for _ in 1..=16 {
        let new_future = create_object(s3_client.clone(), bucket_name.clone(), object_count);
        join_handle_list.push(tokio::spawn(new_future));
    }
    join_all(join_handle_list).await;

}



async fn operation_cleanup_bucket(s3_client: Client, bucket_name: String) {
    let mut delete_tasks = vec![]; // Holds the JoinHandle instances to delete all object batches

    let mut page_token = None;
    loop {
        let mut object_query = s3_client.clone().list_objects_v2()
            .bucket(&bucket_name)
            .max_keys(20);

        if page_token != None {
            object_query = object_query.continuation_token(&page_token.unwrap_or_default());
        }
            
        let object_list = object_query.send().await.unwrap();

        page_token = object_list.next_continuation_token.clone();
        // println!("Next page token is: {0}", page_token.clone().unwrap_or_default());
        let key_count = object_list.key_count.unwrap();

        // Delete any objects returned in the request
        let new_join_handle = tokio::spawn(
            delete_objects(s3_client.clone(), bucket_name.clone(), object_list)
        );
        delete_tasks.push(new_join_handle);
        println!("Spawned a new delete task for {0} objects", key_count);
        if page_token == None { break; }
    }
    
    // join_all(delete_tasks).await;
}


// Deletes the specified batch of objects from an Amazon S3 bucket
async fn delete_objects(s3_client: Client, bucket_name: String, object_list: ListObjectsV2Output) {
    for object in object_list.contents.unwrap() {
        let _delete_result = s3_client.delete_object()
            .bucket(&bucket_name)
            .key(object.key.unwrap())
            .send().await;
    }

}

async fn create_object(s3_client: Client, bucket_name: String, object_count: u32) {
    for _ in 1..=object_count {
        let key = uuid::Uuid::new_v4().to_string();

        let body = ByteStream::new(SdkBody::from(key.clone()));
        
        let put_result = s3_client.put_object()
            .bucket(&bucket_name)
            .key(key)
            .body(body)
            .send().await;

        if put_result.is_err() {
            println!("Failed to create S3 object");
        }
    }
}

fn validate_number(input: &str) -> Result<Validation, CustomUserError> {
    let regex = regex::Regex::new(r"^\d{1,6}$").unwrap();
    if regex.is_match(input) {
        return Ok(Validation::Valid);
    }
    Ok(Validation::Invalid("Invalid quantity specified. Please use a value from 1 - 999999".into()))
}

async fn select_authentication() -> SdkConfig {
    
    let auth_options = vec!["Default", "Environment Variables", "Profile", "SSO"];
    let auth_selection = inquire::Select::new("Select AWS authentication option", auth_options).prompt().unwrap();

    if auth_selection == "Profile" {
        let profile_name = select_profile().await;
        return aws_config::from_env().profile_name(profile_name).load().await;
    }
    else if auth_selection == "SSO" {
        let sso_profile = select_sso_profile().await;
        return aws_config::from_env().profile_name(sso_profile).load().await;
    }
    else if auth_selection == "Environment Variables" {
        dotenvy::dotenv().unwrap();
    }
    return aws_config::load_from_env().await;
}

async fn select_sso_profile() -> String {
    let loaded_profiles = get_aws_env_config_sections().await;
    let prompt = "Select an SSO profile";
    let profile_names = loaded_profiles.sso_sessions().into_iter().map(|x| x.to_string()).collect();

    inquire::Select::new(prompt, profile_names).prompt().unwrap()
}

async fn get_aws_env_config_sections() -> aws_config::profile::ProfileSet  {
    let aws_creds = Builder::new().include_default_credentials_file(true).build();
    
    let fs = aws_types::os_shim_internal::Fs::real();
    let env = aws_types::os_shim_internal::Env::real();

    let loaded_profiles = aws_config::profile::load(&fs, &env, &aws_creds, None).await.unwrap();
    return loaded_profiles;
}

async fn select_profile() -> String {
    let loaded_profiles = get_aws_env_config_sections().await;
    let profile_names: Vec<&str> = loaded_profiles.profiles().collect();
    let prompt = "Select an AWS profile";
    inquire::Select::new(prompt, profile_names).prompt().unwrap().to_string()
}

