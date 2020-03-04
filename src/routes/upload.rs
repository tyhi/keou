use crate::{
    routes::delete::del_file,
    utils::{database, models},
    Settings,
};
use actix_multipart::Multipart;
use actix_web::{
    error,
    http::{header::ContentDisposition, HeaderMap},
    post,
    web::Data,
    HttpRequest, HttpResponse, Result,
};
use async_std::prelude::*;
use futures::StreamExt;
use serde::Serialize;
use sqlx::PgPool;

#[derive(Serialize)]
struct UploadResp {
    url: String,
    delete_url: String,
}

struct NamedReturn {
    new_path: String,
    temp_path: String,
    uri: String,
    ext: String,
}

const RANDOM_FILE_EXT: &[&str] = &["png", "jpeg", "jpg", "webm", "gif", "avi", "mp4"];

#[allow(clippy::cast_precision_loss, clippy::as_conversions)]
#[post("")]
pub async fn upload(
    mut multipart: Multipart,
    config: Data<Settings>,
    request: HttpRequest,
    p: Data<PgPool>,
) -> Result<HttpResponse> {
    let user = check_header(request.headers(), Data::clone(&p))
        .await
        .map_err(error::ErrorUnauthorized)?;

    // Handle multipart upload(s)
    while let Some(item) = multipart.next().await {
        match item {
            Err(_) => (),
            Ok(mut file) => {
                let content = file
                    .content_disposition()
                    .ok_or_else(|| actix_web::error::ParseError::Incomplete)?;

                if !check_name(&content, config.multipart_name.as_str())
                    .map_err(error::ErrorInternalServerError)?
                {
                    continue;
                }

                let file_names = gen_upload_file(&content).await.map_err(|err| {
                    error::ErrorInternalServerError(format!("error generating filename: {}", err))
                })?;

                // Create the temp. file to work with wile we iter over all the chunks.
                let mut f = async_std::fs::File::create(&file_names.temp_path).await?;

                // fs keeps track of how big the file is.
                let mut fs: f64 = 0.0;
                // iter over all chunks we get from client.
                while let Some(chunk) = file.next().await {
                    let data = chunk?;
                    f.write_all(&data).await?;

                    fs += data.len() as f64;

                    // Hard code 95MB upload limit to play nice with cloudflare.
                    // Actual limit is 100MB however we might not be able to catch it before a chunk
                    // might put it over the limit.
                    if fs > 95_000_000.0 {
                        if let Err(err) =
                            del_file(async_std::path::Path::new(&file_names.temp_path)).await
                        {
                            return Err(error::ErrorInternalServerError(format!(
                                "file larger than 90MB & failed to clean temp file: {}",
                                err
                            )));
                        }
                        return Err(error::ErrorPayloadTooLarge("larger than 100mb limit"));
                    }
                }

                // Generates the delete key.
                let del_key = nanoid::nanoid!(12, &nanoid::alphabet::SAFE);

                // We rename in case something goes wrong.
                async_std::fs::rename(&file_names.temp_path, &file_names.new_path).await?;
                let domain = format!(
                    "{}://{}",
                    request.connection_info().scheme(),
                    request.connection_info().host()
                );

                database::insert_file(
                    p,
                    models::InsertFile {
                        owner: user.username,
                        uploaded: chrono::Utc::now().naive_utc(),
                        path: format!("{}.{}", file_names.uri, file_names.ext),
                        deletekey: &del_key,
                        filesize: (fs / 1_000_000.0),
                        downloads: 0,
                    },
                )
                .await
                .map_err(error::ErrorInternalServerError)?;

                return Ok(HttpResponse::Ok().json(&UploadResp {
                    url: format!("{}{}.{}", domain, file_names.uri, file_names.ext),
                    delete_url: format!("{}/d/{}", domain, del_key),
                }));
            },
        }
    }
    Err(error::ErrorBadRequest("no files uploaded"))
}

// Takes the input file name and generates the correct paths needed.
async fn gen_upload_file(
    content: &ContentDisposition,
) -> Result<NamedReturn, Box<dyn std::error::Error>> {
    let filen = str::replace(
        content
            .get_filename()
            .ok_or_else(|| "error getting filename")?,
        " ",
        "_",
    );

    let path = std::path::Path::new(&filen);

    let file_name = path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| "no file_name")?;

    let extension = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| "no extension")?
        .to_ascii_lowercase();

    // This loop makes sure that we don't have collision in file names.
    loop {
        // Cheaking to see if our file needs to have random name.
        let name = if RANDOM_FILE_EXT.iter().any(|x| x == &extension) {
            nanoid::nanoid!(6, &nanoid::alphabet::SAFE)
        } else {
            file_name.to_owned()
        };

        // Creating our random folder name.
        let folder_dir = nanoid::nanoid!(3, &nanoid::alphabet::SAFE);

        let path = format!("./uploads/{}/{}.{}", folder_dir, name, extension);

        if !async_std::path::Path::new(&path).exists().await {
            if !async_std::path::Path::new(&format!("./uploads/{}", folder_dir))
                .exists()
                .await
            {
                async_std::fs::create_dir_all(format!("./uploads/{}", folder_dir)).await?;
            }

            return Ok(NamedReturn {
                new_path: path,
                temp_path: format!("./uploads/{}/{}.{}.~tmp", folder_dir, name, extension),
                uri: format!("/{}/{}", folder_dir, name),
                ext: extension.to_string(),
            });
        }
    }
}

// check_name checks to make sure we have a multipart name.
fn check_name(field: &ContentDisposition, name: &str) -> Result<bool, Box<dyn std::error::Error>> {
    if field
        .get_name()
        .ok_or_else(|| "error getting multipart name")?
        != name
    {
        return Ok(false);
    }
    Ok(true)
}

// Checks headers to see if key is valid.
async fn check_header(
    header: &HeaderMap,
    p: Data<PgPool>,
) -> Result<models::User, Box<dyn std::error::Error>> {
    let apikey = header
        .get("apikey")
        .map_or("", |s| s.to_str().unwrap_or(""))
        .to_string();

    if database::check_api(Data::clone(&p), &apikey).await? {
        return Ok(database::get_user(p, apikey).await?);
    }

    Err("invalid api_key".into())
}
