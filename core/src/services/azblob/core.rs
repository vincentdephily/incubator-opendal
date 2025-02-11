// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::fmt;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::fmt::Write;
use std::time::Duration;

use http::header::HeaderName;
use http::header::CONTENT_LENGTH;
use http::header::CONTENT_TYPE;
use http::header::IF_MATCH;
use http::header::IF_NONE_MATCH;
use http::HeaderValue;
use http::Request;
use http::Response;
use reqsign::AzureStorageCredential;
use reqsign::AzureStorageLoader;
use reqsign::AzureStorageSigner;
use serde::Deserialize;

use crate::raw::*;
use crate::*;

mod constants {
    pub const X_MS_VERSION: &str = "x-ms-version";

    pub const X_MS_BLOB_TYPE: &str = "x-ms-blob-type";
    pub const X_MS_COPY_SOURCE: &str = "x-ms-copy-source";
    pub const X_MS_BLOB_CACHE_CONTROL: &str = "x-ms-blob-cache-control";
    pub const X_MS_BLOB_CONDITION_APPENDPOS: &str = "x-ms-blob-condition-appendpos";

    // Server-side encryption with customer-provided headers
    pub const X_MS_ENCRYPTION_KEY: &str = "x-ms-encryption-key";
    pub const X_MS_ENCRYPTION_KEY_SHA256: &str = "x-ms-encryption-key-sha256";
    pub const X_MS_ENCRYPTION_ALGORITHM: &str = "x-ms-encryption-algorithm";
}

pub struct AzblobCore {
    pub container: String,
    pub root: String,
    pub endpoint: String,
    pub encryption_key: Option<HeaderValue>,
    pub encryption_key_sha256: Option<HeaderValue>,
    pub encryption_algorithm: Option<HeaderValue>,
    pub client: HttpClient,
    pub loader: AzureStorageLoader,
    pub signer: AzureStorageSigner,
    pub batch_max_operations: usize,
}

impl Debug for AzblobCore {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("AzblobCore")
            .field("container", &self.container)
            .field("root", &self.root)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl AzblobCore {
    async fn load_credential(&self) -> Result<AzureStorageCredential> {
        let cred = self
            .loader
            .load()
            .await
            .map_err(new_request_credential_error)?;

        if let Some(cred) = cred {
            Ok(cred)
        } else {
            Err(Error::new(
                ErrorKind::ConfigInvalid,
                "no valid credential found",
            ))
        }
    }

    pub async fn sign_query<T>(&self, req: &mut Request<T>) -> Result<()> {
        let cred = self.load_credential().await?;

        self.signer
            .sign_query(req, Duration::from_secs(3600), &cred)
            .map_err(new_request_sign_error)
    }

    pub async fn sign<T>(&self, req: &mut Request<T>) -> Result<()> {
        let cred = self.load_credential().await?;
        // Insert x-ms-version header for normal requests.
        req.headers_mut().insert(
            HeaderName::from_static(constants::X_MS_VERSION),
            // 2022-11-02 is the version supported by Azurite V3 and
            // used by Azure Portal, We use this version to make
            // sure most our developer happy.
            //
            // In the future, we could allow users to configure this value.
            HeaderValue::from_static("2022-11-02"),
        );
        self.signer.sign(req, &cred).map_err(new_request_sign_error)
    }

    async fn batch_sign<T>(&self, req: &mut Request<T>) -> Result<()> {
        let cred = self.load_credential().await?;
        self.signer.sign(req, &cred).map_err(new_request_sign_error)
    }

    #[inline]
    pub async fn send(&self, req: Request<AsyncBody>) -> Result<Response<IncomingAsyncBody>> {
        self.client.send(req).await
    }

    pub fn insert_sse_headers(&self, mut req: http::request::Builder) -> http::request::Builder {
        if let Some(v) = &self.encryption_key {
            let mut v = v.clone();
            v.set_sensitive(true);

            req = req.header(HeaderName::from_static(constants::X_MS_ENCRYPTION_KEY), v)
        }

        if let Some(v) = &self.encryption_key_sha256 {
            let mut v = v.clone();
            v.set_sensitive(true);

            req = req.header(
                HeaderName::from_static(constants::X_MS_ENCRYPTION_KEY_SHA256),
                v,
            )
        }

        if let Some(v) = &self.encryption_algorithm {
            let mut v = v.clone();
            v.set_sensitive(true);

            req = req.header(
                HeaderName::from_static(constants::X_MS_ENCRYPTION_ALGORITHM),
                v,
            )
        }

        req
    }
}

impl AzblobCore {
    pub fn azblob_get_blob_request(&self, path: &str, args: &OpRead) -> Result<Request<AsyncBody>> {
        let p = build_abs_path(&self.root, path);

        let mut url = format!(
            "{}/{}/{}",
            self.endpoint,
            self.container,
            percent_encode_path(&p)
        );

        let mut query_args = Vec::new();
        if let Some(override_content_disposition) = args.override_content_disposition() {
            query_args.push(format!(
                "rscd={}",
                percent_encode_path(override_content_disposition)
            ))
        }

        if !query_args.is_empty() {
            url.push_str(&format!("?{}", query_args.join("&")));
        }

        let mut req = Request::get(&url);

        // Set SSE headers.
        req = self.insert_sse_headers(req);

        let range = args.range();
        if !range.is_full() {
            // azblob doesn't support read with suffix range.
            //
            // ref: https://learn.microsoft.com/en-us/rest/api/storageservices/specifying-the-range-header-for-blob-service-operations
            if range.offset().is_none() && range.size().is_some() {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "azblob doesn't support read with suffix range",
                ));
            }

            req = req.header(http::header::RANGE, range.to_header());
        }

        if let Some(if_none_match) = args.if_none_match() {
            req = req.header(IF_NONE_MATCH, if_none_match);
        }

        if let Some(if_match) = args.if_match() {
            req = req.header(IF_MATCH, if_match);
        }

        let req = req
            .body(AsyncBody::Empty)
            .map_err(new_request_build_error)?;

        Ok(req)
    }

    pub async fn azblob_get_blob(
        &self,
        path: &str,
        args: &OpRead,
    ) -> Result<Response<IncomingAsyncBody>> {
        let mut req = self.azblob_get_blob_request(path, args)?;

        self.sign(&mut req).await?;

        self.send(req).await
    }

    pub fn azblob_put_blob_request(
        &self,
        path: &str,
        size: Option<u64>,
        args: &OpWrite,
        body: AsyncBody,
    ) -> Result<Request<AsyncBody>> {
        let p = build_abs_path(&self.root, path);

        let url = format!(
            "{}/{}/{}",
            self.endpoint,
            self.container,
            percent_encode_path(&p)
        );

        let mut req = Request::put(&url);

        // Set SSE headers.
        req = self.insert_sse_headers(req);

        if let Some(cache_control) = args.cache_control() {
            req = req.header(constants::X_MS_BLOB_CACHE_CONTROL, cache_control);
        }
        if let Some(size) = size {
            req = req.header(CONTENT_LENGTH, size)
        }

        if let Some(ty) = args.content_type() {
            req = req.header(CONTENT_TYPE, ty)
        }

        req = req.header(
            HeaderName::from_static(constants::X_MS_BLOB_TYPE),
            "BlockBlob",
        );

        // Set body
        let req = req.body(body).map_err(new_request_build_error)?;

        Ok(req)
    }

    /// For appendable object, it could be created by `put` an empty blob
    /// with `x-ms-blob-type` header set to `AppendBlob`.
    /// And it's just initialized with empty content.
    ///
    /// If want to append content to it, we should use the following method `azblob_append_blob_request`.
    ///
    /// # Notes
    ///
    /// Appendable blob's custom header could only be set when it's created.
    ///
    /// The following custom header could be set:
    /// - `content-type`
    /// - `x-ms-blob-cache-control`
    ///
    /// # Reference
    ///
    /// https://learn.microsoft.com/en-us/rest/api/storageservices/put-blob
    pub fn azblob_init_appendable_blob_request(
        &self,
        path: &str,
        args: &OpWrite,
    ) -> Result<Request<AsyncBody>> {
        let p = build_abs_path(&self.root, path);

        let url = format!(
            "{}/{}/{}",
            self.endpoint,
            self.container,
            percent_encode_path(&p)
        );

        let mut req = Request::put(&url);

        // Set SSE headers.
        req = self.insert_sse_headers(req);

        // The content-length header must be set to zero
        // when creating an appendable blob.
        req = req.header(CONTENT_LENGTH, 0);
        req = req.header(
            HeaderName::from_static(constants::X_MS_BLOB_TYPE),
            "AppendBlob",
        );

        if let Some(ty) = args.content_type() {
            req = req.header(CONTENT_TYPE, ty)
        }

        if let Some(cache_control) = args.cache_control() {
            req = req.header(constants::X_MS_BLOB_CACHE_CONTROL, cache_control);
        }

        let req = req
            .body(AsyncBody::Empty)
            .map_err(new_request_build_error)?;

        Ok(req)
    }

    /// Append content to an appendable blob.
    /// The content will be appended to the end of the blob.
    ///
    /// # Notes
    ///
    /// - The maximum size of the content could be appended is 4MB.
    /// - `Append Block` succeeds only if the blob already exists.
    ///
    /// # Reference
    ///
    /// https://learn.microsoft.com/en-us/rest/api/storageservices/append-block
    pub fn azblob_append_blob_request(
        &self,
        path: &str,
        position: u64,
        size: u64,
        body: AsyncBody,
    ) -> Result<Request<AsyncBody>> {
        let p = build_abs_path(&self.root, path);

        let url = format!(
            "{}/{}/{}?comp=appendblock",
            self.endpoint,
            self.container,
            percent_encode_path(&p)
        );

        let mut req = Request::put(&url);

        // Set SSE headers.
        req = self.insert_sse_headers(req);

        req = req.header(CONTENT_LENGTH, size);

        req = req.header(constants::X_MS_BLOB_CONDITION_APPENDPOS, position);

        let req = req.body(body).map_err(new_request_build_error)?;

        Ok(req)
    }

    pub fn azblob_head_blob_request(
        &self,
        path: &str,
        args: &OpStat,
    ) -> Result<Request<AsyncBody>> {
        let p = build_abs_path(&self.root, path);

        let url = format!(
            "{}/{}/{}",
            self.endpoint,
            self.container,
            percent_encode_path(&p)
        );

        let mut req = Request::head(&url);

        // Set SSE headers.
        req = self.insert_sse_headers(req);

        if let Some(if_none_match) = args.if_none_match() {
            req = req.header(IF_NONE_MATCH, if_none_match);
        }

        if let Some(if_match) = args.if_match() {
            req = req.header(IF_MATCH, if_match);
        }

        let req = req
            .body(AsyncBody::Empty)
            .map_err(new_request_build_error)?;

        Ok(req)
    }

    pub async fn azblob_get_blob_properties(
        &self,
        path: &str,
        args: &OpStat,
    ) -> Result<Response<IncomingAsyncBody>> {
        let mut req = self.azblob_head_blob_request(path, args)?;

        self.sign(&mut req).await?;
        self.send(req).await
    }

    pub fn azblob_delete_blob_request(&self, path: &str) -> Result<Request<AsyncBody>> {
        let p = build_abs_path(&self.root, path);

        let url = format!(
            "{}/{}/{}",
            self.endpoint,
            self.container,
            percent_encode_path(&p)
        );

        let req = Request::delete(&url);

        req.header(CONTENT_LENGTH, 0)
            .body(AsyncBody::Empty)
            .map_err(new_request_build_error)
    }

    pub async fn azblob_delete_blob(&self, path: &str) -> Result<Response<IncomingAsyncBody>> {
        let mut req = self.azblob_delete_blob_request(path)?;

        self.sign(&mut req).await?;
        self.send(req).await
    }

    pub async fn azblob_copy_blob(
        &self,
        from: &str,
        to: &str,
    ) -> Result<Response<IncomingAsyncBody>> {
        let source = build_abs_path(&self.root, from);
        let target = build_abs_path(&self.root, to);

        let source = format!(
            "{}/{}/{}",
            self.endpoint,
            self.container,
            percent_encode_path(&source)
        );
        let target = format!(
            "{}/{}/{}",
            self.endpoint,
            self.container,
            percent_encode_path(&target)
        );

        let mut req = Request::put(&target)
            .header(constants::X_MS_COPY_SOURCE, source)
            .header(CONTENT_LENGTH, 0)
            .body(AsyncBody::Empty)
            .map_err(new_request_build_error)?;

        self.sign(&mut req).await?;
        self.send(req).await
    }

    pub async fn azblob_list_blobs(
        &self,
        path: &str,
        next_marker: &str,
        delimiter: &str,
        limit: Option<usize>,
    ) -> Result<Response<IncomingAsyncBody>> {
        let p = build_abs_path(&self.root, path);

        let mut url = format!(
            "{}/{}?restype=container&comp=list",
            self.endpoint, self.container
        );
        if !p.is_empty() {
            write!(url, "&prefix={}", percent_encode_path(&p))
                .expect("write into string must succeed");
        }
        if let Some(limit) = limit {
            write!(url, "&maxresults={limit}").expect("write into string must succeed");
        }
        if !delimiter.is_empty() {
            write!(url, "&delimiter={delimiter}").expect("write into string must succeed");
        }
        if !next_marker.is_empty() {
            write!(url, "&marker={next_marker}").expect("write into string must succeed");
        }

        let mut req = Request::get(&url)
            .body(AsyncBody::Empty)
            .map_err(new_request_build_error)?;

        self.sign(&mut req).await?;
        self.send(req).await
    }

    pub async fn azblob_batch_delete(
        &self,
        paths: &[String],
    ) -> Result<Response<IncomingAsyncBody>> {
        let url = format!(
            "{}/{}?restype=container&comp=batch",
            self.endpoint, self.container
        );

        let mut multipart = Multipart::new();

        for (idx, path) in paths.iter().enumerate() {
            let mut req = self.azblob_delete_blob_request(path)?;
            self.batch_sign(&mut req).await?;

            multipart = multipart.part(
                MixedPart::from_request(req).part_header("content-id".parse().unwrap(), idx.into()),
            );
        }

        let req = Request::post(url);
        let mut req = multipart.apply(req)?;

        self.sign(&mut req).await?;
        self.send(req).await
    }
}

#[derive(Default, Debug, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ListBlobsOutput {
    pub blobs: Blobs,
    #[serde(rename = "NextMarker")]
    pub next_marker: Option<String>,
}

#[derive(Default, Debug, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Blobs {
    pub blob: Vec<Blob>,
    pub blob_prefix: Vec<BlobPrefix>,
}

#[derive(Default, Debug, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct BlobPrefix {
    pub name: String,
}

#[derive(Default, Debug, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Blob {
    pub properties: Properties,
    pub name: String,
}

#[derive(Default, Debug, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Properties {
    #[serde(rename = "Content-Length")]
    pub content_length: u64,
    #[serde(rename = "Last-Modified")]
    pub last_modified: String,
    #[serde(rename = "Content-MD5")]
    pub content_md5: String,
    #[serde(rename = "Content-Type")]
    pub content_type: String,
    pub etag: String,
}

#[cfg(test)]
mod tests {
    use bytes::Buf;
    use bytes::Bytes;
    use quick_xml::de;

    use super::*;

    #[test]
    fn test_parse_xml() {
        let bs = bytes::Bytes::from(
            r#"
            <?xml version="1.0" encoding="utf-8"?>
            <EnumerationResults ServiceEndpoint="https://test.blob.core.windows.net/" ContainerName="myazurebucket">
                <Prefix>dir1/</Prefix>
                <Delimiter>/</Delimiter>
                <Blobs>
                    <Blob>
                        <Name>dir1/2f018bb5-466f-4af1-84fa-2b167374ee06</Name>
                        <Properties>
                            <Creation-Time>Sun, 20 Mar 2022 11:29:03 GMT</Creation-Time>
                            <Last-Modified>Sun, 20 Mar 2022 11:29:03 GMT</Last-Modified>
                            <Etag>0x8DA0A64D66790C3</Etag>
                            <Content-Length>3485277</Content-Length>
                            <Content-Type>application/octet-stream</Content-Type>
                            <Content-Encoding />
                            <Content-Language />
                            <Content-CRC64 />
                            <Content-MD5>llJ/+jOlx5GdA1sL7SdKuw==</Content-MD5>
                            <Cache-Control />
                            <Content-Disposition />
                            <BlobType>BlockBlob</BlobType>
                            <AccessTier>Hot</AccessTier>
                            <AccessTierInferred>true</AccessTierInferred>
                            <LeaseStatus>unlocked</LeaseStatus>
                            <LeaseState>available</LeaseState>
                            <ServerEncrypted>true</ServerEncrypted>
                        </Properties>
                        <OrMetadata />
                    </Blob>
                    <Blob>
                        <Name>dir1/5b9432b2-79c0-48d8-90c2-7d3e153826ed</Name>
                        <Properties>
                            <Creation-Time>Tue, 29 Mar 2022 01:54:07 GMT</Creation-Time>
                            <Last-Modified>Tue, 29 Mar 2022 01:54:07 GMT</Last-Modified>
                            <Etag>0x8DA112702D88FE4</Etag>
                            <Content-Length>2471869</Content-Length>
                            <Content-Type>application/octet-stream</Content-Type>
                            <Content-Encoding />
                            <Content-Language />
                            <Content-CRC64 />
                            <Content-MD5>xmgUltSnopLSJOukgCHFtg==</Content-MD5>
                            <Cache-Control />
                            <Content-Disposition />
                            <BlobType>BlockBlob</BlobType>
                            <AccessTier>Hot</AccessTier>
                            <AccessTierInferred>true</AccessTierInferred>
                            <LeaseStatus>unlocked</LeaseStatus>
                            <LeaseState>available</LeaseState>
                            <ServerEncrypted>true</ServerEncrypted>
                        </Properties>
                        <OrMetadata />
                    </Blob>
                    <Blob>
                        <Name>dir1/b2d96f8b-d467-40d1-bb11-4632dddbf5b5</Name>
                        <Properties>
                            <Creation-Time>Sun, 20 Mar 2022 11:31:57 GMT</Creation-Time>
                            <Last-Modified>Sun, 20 Mar 2022 11:31:57 GMT</Last-Modified>
                            <Etag>0x8DA0A653DC82981</Etag>
                            <Content-Length>1259677</Content-Length>
                            <Content-Type>application/octet-stream</Content-Type>
                            <Content-Encoding />
                            <Content-Language />
                            <Content-CRC64 />
                            <Content-MD5>AxTiFXHwrXKaZC5b7ZRybw==</Content-MD5>
                            <Cache-Control />
                            <Content-Disposition />
                            <BlobType>BlockBlob</BlobType>
                            <AccessTier>Hot</AccessTier>
                            <AccessTierInferred>true</AccessTierInferred>
                            <LeaseStatus>unlocked</LeaseStatus>
                            <LeaseState>available</LeaseState>
                            <ServerEncrypted>true</ServerEncrypted>
                        </Properties>
                        <OrMetadata />
                    </Blob>
                    <BlobPrefix>
                        <Name>dir1/dir2/</Name>
                    </BlobPrefix>
                    <BlobPrefix>
                        <Name>dir1/dir21/</Name>
                    </BlobPrefix>
                </Blobs>
                <NextMarker />
            </EnumerationResults>"#,
        );
        let out: ListBlobsOutput = de::from_reader(bs.reader()).expect("must success");
        println!("{out:?}");

        assert_eq!(
            out.blobs
                .blob
                .iter()
                .map(|v| v.name.clone())
                .collect::<Vec<String>>(),
            [
                "dir1/2f018bb5-466f-4af1-84fa-2b167374ee06",
                "dir1/5b9432b2-79c0-48d8-90c2-7d3e153826ed",
                "dir1/b2d96f8b-d467-40d1-bb11-4632dddbf5b5"
            ]
        );
        assert_eq!(
            out.blobs
                .blob
                .iter()
                .map(|v| v.properties.content_length)
                .collect::<Vec<u64>>(),
            [3485277, 2471869, 1259677]
        );
        assert_eq!(
            out.blobs
                .blob
                .iter()
                .map(|v| v.properties.content_md5.clone())
                .collect::<Vec<String>>(),
            [
                "llJ/+jOlx5GdA1sL7SdKuw==".to_string(),
                "xmgUltSnopLSJOukgCHFtg==".to_string(),
                "AxTiFXHwrXKaZC5b7ZRybw==".to_string()
            ]
        );
        assert_eq!(
            out.blobs
                .blob
                .iter()
                .map(|v| v.properties.last_modified.clone())
                .collect::<Vec<String>>(),
            [
                "Sun, 20 Mar 2022 11:29:03 GMT".to_string(),
                "Tue, 29 Mar 2022 01:54:07 GMT".to_string(),
                "Sun, 20 Mar 2022 11:31:57 GMT".to_string()
            ]
        );
        assert_eq!(
            out.blobs
                .blob
                .iter()
                .map(|v| v.properties.etag.clone())
                .collect::<Vec<String>>(),
            [
                "0x8DA0A64D66790C3".to_string(),
                "0x8DA112702D88FE4".to_string(),
                "0x8DA0A653DC82981".to_string()
            ]
        );
        assert_eq!(
            out.blobs
                .blob_prefix
                .iter()
                .map(|v| v.name.clone())
                .collect::<Vec<String>>(),
            ["dir1/dir2/", "dir1/dir21/"]
        );
    }

    /// This case is copied from real environment for testing
    /// quick-xml overlapped-lists features. By default, quick-xml
    /// can't deserialize content with overlapped-lists.
    ///
    /// For example, this case list blobs in this way:
    ///
    /// ```xml
    /// <Blobs>
    ///     <Blob>xxx</Blob>
    ///     <BlobPrefix>yyy</BlobPrefix>
    ///     <Blob>zzz</Blob>
    /// </Blobs>
    /// ```
    ///
    /// If `overlapped-lists` feature not enabled, we will get error `duplicate field Blob`.
    #[test]
    fn test_parse_overlapped_lists() {
        let bs = "<?xml version=\"1.0\" encoding=\"utf-8\"?><EnumerationResults ServiceEndpoint=\"https://test.blob.core.windows.net/\" ContainerName=\"test\"><Prefix>9f7075e1-84d0-45ca-8196-ab9b71a8ef97/x/</Prefix><Delimiter>/</Delimiter><Blobs><Blob><Name>9f7075e1-84d0-45ca-8196-ab9b71a8ef97/x/</Name><Properties><Creation-Time>Thu, 01 Sep 2022 07:26:49 GMT</Creation-Time><Last-Modified>Thu, 01 Sep 2022 07:26:49 GMT</Last-Modified><Etag>0x8DA8BEB55D0EA35</Etag><Content-Length>0</Content-Length><Content-Type>application/octet-stream</Content-Type><Content-Encoding /><Content-Language /><Content-CRC64 /><Content-MD5>1B2M2Y8AsgTpgAmY7PhCfg==</Content-MD5><Cache-Control /><Content-Disposition /><BlobType>BlockBlob</BlobType><AccessTier>Hot</AccessTier><AccessTierInferred>true</AccessTierInferred><LeaseStatus>unlocked</LeaseStatus><LeaseState>available</LeaseState><ServerEncrypted>true</ServerEncrypted></Properties><OrMetadata /></Blob><BlobPrefix><Name>9f7075e1-84d0-45ca-8196-ab9b71a8ef97/x/x/</Name></BlobPrefix><Blob><Name>9f7075e1-84d0-45ca-8196-ab9b71a8ef97/x/y</Name><Properties><Creation-Time>Thu, 01 Sep 2022 07:26:50 GMT</Creation-Time><Last-Modified>Thu, 01 Sep 2022 07:26:50 GMT</Last-Modified><Etag>0x8DA8BEB55D99C08</Etag><Content-Length>0</Content-Length><Content-Type>application/octet-stream</Content-Type><Content-Encoding /><Content-Language /><Content-CRC64 /><Content-MD5>1B2M2Y8AsgTpgAmY7PhCfg==</Content-MD5><Cache-Control /><Content-Disposition /><BlobType>BlockBlob</BlobType><AccessTier>Hot</AccessTier><AccessTierInferred>true</AccessTierInferred><LeaseStatus>unlocked</LeaseStatus><LeaseState>available</LeaseState><ServerEncrypted>true</ServerEncrypted></Properties><OrMetadata /></Blob></Blobs><NextMarker /></EnumerationResults>";

        de::from_reader(Bytes::from(bs).reader()).expect("must success")
    }
}
