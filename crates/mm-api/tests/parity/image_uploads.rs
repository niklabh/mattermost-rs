//! Cross-server parity for the **raster-image** branch of the two file-writing routes:
//! `POST /api/v4/files` (`UploadFileX` → `preprocessImage`/`postprocessImage`) and the completing
//! chunk of `POST /api/v4/uploads/{upload_id}` (`UploadData` → `HandleImages`).
//!
//! ```sh
//! scripts/parity.sh --test parity image_uploads
//! ```
//!
//! What is compared is what Go stores: the `FileInfo` (dimensions after the EXIF swap,
//! `has_preview_image`, the derived paths, and on `POST /files` the base64 `mini_preview`), and the
//! **bytes** of the `_thumb` and `_preview` files, read back through `GET /files/{id}/thumbnail`
//! and `/preview` from the server that wrote them. Both servers share one file-store directory, so
//! a byte difference is a difference in what was written, not in how it is served.
//!
//! The inputs come from the imaging oracle's own corpus (`fixtures/behaviour_imaging_*.json`): real
//! photo-sized JPEGs with and without an axis-swapping EXIF orientation, PNGs of every colour type
//! pngsuite covers, and files whose header decodes but whose body does not — where Go keeps the
//! row, sets `has_preview_image`, and writes no derived file at all.

use crate::common;

use base64::Engine as _;
use common::{
    GO, RUST, TINY_PNG, a_team_and_channel_the_user_is_in, client, go_minted_token,
    purge_api_fixtures, stack_enabled,
};

/// One request to a base, returning `(status, x-mmrs-served-by, body)`.
async fn send(
    client: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    path: &str,
    token: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> (u16, Option<String>, Vec<u8>) {
    let mut request = client
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .body(body);
    if let Some(ct) = content_type {
        request = request.header("Content-Type", ct);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served_by = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(std::borrow::ToOwned::to_owned);
    let body = response.bytes().await.expect("body reads").to_vec();
    (status, served_by, body)
}

/// A decoder input from one of the imaging oracle's fixtures, by stage and case name.
fn corpus_file(stage: &str, name: &str) -> Vec<u8> {
    let path = format!(
        "{}/../../fixtures/behaviour_imaging_{stage}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let value: serde_json::Value = serde_json::from_str(&text).expect("an oracle fixture");
    let cases = match stage {
        "pipeline" | "exif" => &value["cases"],
        _ => &value["decode"],
    };
    let case = cases
        .as_array()
        .expect("a case list")
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("{stage}: no case {name}"));
    base64::engine::general_purpose::STANDARD
        .decode(case["b64"].as_str().expect("the case carries its bytes"))
        .expect("base64")
}

/// The inputs every test here runs: `(label, filename, bytes)`.
fn corpus() -> Vec<(&'static str, &'static str, Vec<u8>)> {
    vec![
        ("tiny png", "mmrs-parity-img-tiny.png", TINY_PNG.to_vec()),
        (
            "photo 2400x1600",
            "mmrs-parity-img-photo.jpg",
            corpus_file("pipeline", "photo_2400x1600"),
        ),
        (
            "photo rotated 90 by EXIF",
            "mmrs-parity-img-photo-o6.jpg",
            corpus_file("pipeline", "photo_2400x1600_o006"),
        ),
        (
            "portrait rotated by EXIF",
            "mmrs-parity-img-portrait-o6.jpeg",
            corpus_file("pipeline", "portrait_900x1950_o6"),
        ),
        (
            "screenshot png",
            "mmrs-parity-img-screen.png",
            corpus_file("pipeline", "screenshot_1440x900"),
        ),
        (
            "alpha png",
            "mmrs-parity-img-alpha.png",
            corpus_file("pipeline", "alpha_1000x300"),
        ),
        (
            "paletted png",
            "mmrs-parity-img-pal.png",
            corpus_file("png", "basn3p08.png"),
        ),
        (
            "16-bit gray png",
            "mmrs-parity-img-g16.png",
            corpus_file("png", "basn0g16.png"),
        ),
        (
            "16-bit gray+alpha png",
            "mmrs-parity-img-ga16.png",
            corpus_file("png", "basn4a16.png"),
        ),
        (
            "progressive jpeg",
            "mmrs-parity-img-prog.jpg",
            corpus_file("jpeg", "video-001.progressive.jpeg"),
        ),
        (
            "4:2:2 jpeg",
            "mmrs-parity-img-422.jpg",
            corpus_file("jpeg", "video-001.q50.422.jpeg"),
        ),
        (
            "cmyk jpeg",
            "mmrs-parity-img-cmyk.jpg",
            corpus_file("jpeg", "video-001.cmyk.jpeg"),
        ),
        (
            "gray jpeg",
            "mmrs-parity-img-gray.jpg",
            corpus_file("jpeg", "video-005.gray.jpeg"),
        ),
        (
            "png with EXIF orientation 5",
            "mmrs-parity-img-exif.png",
            corpus_file("exif", "png_be_005"),
        ),
        // A JPEG named .png: mime image/png names the derived files `_thumb.png`, but the decoder
        // says "jpeg", so Go writes JPEG bytes into them.
        (
            "jpeg named png",
            "mmrs-parity-img-liar.png",
            corpus_file("jpeg", "video-001.q50.420.jpeg"),
        ),
        // A PNG named .gif: the mime type says image/gif, so preprocessImage decodes it whole and
        // clears has_preview_image — but the derived files are still written, as PNG bytes
        // under `_thumb.jpg`, because the decoder said "png" and the mime said "not png".
        (
            "png named gif",
            "mmrs-parity-img-liar.gif",
            TINY_PNG.to_vec(),
        ),
        // Header decodes, body does not: the row keeps its preview paths and nothing is written.
        (
            "truncated png",
            "mmrs-parity-img-cut.png",
            corpus_file("png", "cut_4388"),
        ),
        (
            "jpeg with a bad Huffman table class",
            "mmrs-parity-img-badtc.jpg",
            corpus_file("jpeg", "flip_160"),
        ),
        (
            "truncated jpeg",
            "mmrs-parity-img-cut.jpg",
            corpus_file("jpeg", "cut_723"),
        ),
        // Damaged entropy data that still decodes: garbage pixels, but pixels.
        (
            "bit-flipped jpeg",
            "mmrs-parity-img-flip.jpg",
            corpus_file("jpeg", "flip_330"),
        ),
        // GIF and BMP, served since `goimage::gif` and `goimage::bmp` landed. The mime table
        // calls a `.gif` `image/gif`, which is the one branch of `preprocessImage` that decodes
        // the whole file and clears `has_preview_image` — so the animated case exercises both
        // that and the fact that `image.Decode` yields frame 0 alone.
        (
            "gif photo",
            "mmrs-parity-img-photo.gif",
            corpus_file("pipeline", "gif_1400x900"),
        ),
        (
            "animated gif",
            "mmrs-parity-img-anim.gif",
            corpus_file("pipeline", "gif_animated_300x200"),
        ),
        (
            "interlaced gif",
            "mmrs-parity-img-inter.gif",
            corpus_file("gif", "crafted_interlaced_17"),
        ),
        (
            "24-bit bmp photo",
            "mmrs-parity-img-photo.bmp",
            corpus_file("pipeline", "bmp_1400x900_24"),
        ),
        (
            "8-bit gray bmp",
            "mmrs-parity-img-gray.bmp",
            corpus_file("pipeline", "bmp_2000x120_gray"),
        ),
        (
            "1-bit paletted bmp",
            "mmrs-parity-img-1bpp.bmp",
            corpus_file("bmp", "bmp_1bpp.bmp"),
        ),
        (
            "32-bit bmp with alpha",
            "mmrs-parity-img-alpha.bmp",
            corpus_file("bmp", "crafted_baseline_32_topdown"),
        ),
    ]
}

/// Drop the fields two independent uploads cannot share, and the file id inside the three paths.
fn normalise(mut info: serde_json::Value) -> serde_json::Value {
    let id = info["id"].as_str().unwrap_or_default().to_owned();
    if let Some(obj) = info.as_object_mut() {
        for key in ["id", "create_at", "update_at", "post_id", "channel_id"] {
            obj.remove(key);
        }
        for key in ["path", "thumbnail_path", "preview_path"] {
            if let Some(serde_json::Value::String(path)) = obj.get_mut(key) {
                if !id.is_empty() {
                    *path = path.replace(&id, "<id>");
                }
            }
        }
    }
    info
}

/// The three stored paths of a FileInfo row — `json:"-"` in Go, so never on the wire and only
/// readable from the table — reduced to what two uploads can share: the thumbnail's and preview's
/// file names, and whether each sits in the original's directory.
async fn stored_paths(file_id: &str) -> serde_json::Value {
    let url = std::env::var("DATABASE_URL").expect("the parity stack's database");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("the database answers");
    let (path, thumb, preview): (String, String, String) =
        sqlx::query_as("SELECT path, thumbnailpath, previewpath FROM fileinfo WHERE id = $1")
            .bind(file_id)
            .fetch_one(&pool)
            .await
            .expect("the row exists");
    let dir = |p: &str| {
        p.rsplit_once('/')
            .map_or(String::new(), |(d, _)| d.to_owned())
    };
    let base = |p: &str| {
        p.rsplit_once('/')
            .map_or(p.to_owned(), |(_, b)| b.to_owned())
    };
    serde_json::json!({
        "thumb_name": base(&thumb),
        "preview_name": base(&preview),
        "thumb_beside_original": !thumb.is_empty() && dir(&thumb) == dir(&path),
        "preview_beside_original": !preview.is_empty() && dir(&preview) == dir(&path),
    })
}

/// `GET /files/{id}/thumbnail` and `/preview` from one server: the status and bytes of each.
async fn derived(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    file_id: &str,
) -> Vec<(u16, Vec<u8>)> {
    let mut out = Vec::new();
    for kind in ["thumbnail", "preview"] {
        let (status, _, body) = send(
            client,
            base,
            reqwest::Method::GET,
            &format!("/api/v4/files/{file_id}/{kind}"),
            token,
            None,
            Vec::new(),
        )
        .await;
        out.push((status, if status == 200 { body } else { Vec::new() }));
    }
    out
}

fn first_info(body: &[u8]) -> serde_json::Value {
    let value: serde_json::Value = serde_json::from_slice(body).expect("a FileUploadResponse");
    value["file_infos"][0].clone()
}

/// Every corpus image through the simple-body `POST /files` on both servers: the row, the mini
/// preview and both derived files are byte-identical, and Rust served it.
#[tokio::test]
async fn every_corpus_image_uploads_identically() {
    if !stack_enabled() {
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&client, &token).await;

    let mut with_derived = 0;
    let mut with_mini = 0;
    for (label, filename, bytes) in corpus() {
        let path = format!("/api/v4/files?channel_id={channel}&filename={filename}");
        let mut seen = Vec::new();
        for base in [GO, RUST] {
            let (status, served_by, body) = send(
                &client,
                base,
                reqwest::Method::POST,
                &path,
                &token,
                Some("application/octet-stream"),
                bytes.clone(),
            )
            .await;
            assert_eq!(
                status,
                201,
                "{label} {base}: {}",
                String::from_utf8_lossy(&body)
            );
            if base == RUST {
                assert_eq!(
                    served_by.as_deref(),
                    Some("rust"),
                    "{label}: served by rust"
                );
            }
            let info = first_info(&body);
            let file_id = info["id"].as_str().expect("a file id").to_owned();
            let files = derived(&client, base, &token, &file_id).await;
            let mut row = normalise(info);
            row["stored_paths"] = stored_paths(&file_id).await;
            seen.push((row, files));
        }
        assert_eq!(seen[0].0, seen[1].0, "{label}: the FileInfo rows differ");
        assert_eq!(
            seen[0].1[0], seen[1].1[0],
            "{label}: the stored thumbnails differ (status, bytes)"
        );
        assert_eq!(
            seen[0].1[1], seen[1].1[1],
            "{label}: the stored previews differ (status, bytes)"
        );
        if seen[1]
            .1
            .iter()
            .all(|(status, body)| *status == 200 && !body.is_empty())
        {
            with_derived += 1;
        }
        if seen[1].0["mini_preview"].is_string() {
            with_mini += 1;
        }
    }
    // Guard against a vacuous pass: every corpus file but the three whose bodies do not decode
    // must have produced both derived files and a mini preview — including the PNG named .gif,
    // whose row says has_preview_image: false.
    let total = corpus().len();
    assert_eq!(
        with_derived,
        total - 3,
        "files with a thumbnail and a preview"
    );
    assert_eq!(with_mini, total - 3, "files with a mini preview");
}

/// A multipart body with an image and a text file is served whole, and matches Go.
#[tokio::test]
async fn a_multipart_image_and_text_upload_is_served_and_matches() {
    if !stack_enabled() {
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&client, &token).await;

    const BOUNDARY: &str = "mmrsparityimageboundary";
    let image = corpus_file("pipeline", "blocks_400x300");
    let mut body = Vec::new();
    body.extend_from_slice(
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"channel_id\"\r\n\r\n{channel}\r\n").as_bytes(),
    );
    body.extend_from_slice(
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"mmrs-parity-img-mp.png\"\r\nContent-Type: image/png\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(&image);
    body.extend_from_slice(
        format!("\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"mmrs-parity-img-mp.txt\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n--{BOUNDARY}--\r\n").as_bytes(),
    );
    let content_type = format!("multipart/form-data; boundary={BOUNDARY}");

    let mut seen = Vec::new();
    for base in [GO, RUST] {
        let (status, served_by, reply) = send(
            &client,
            base,
            reqwest::Method::POST,
            "/api/v4/files",
            &token,
            Some(&content_type),
            body.clone(),
        )
        .await;
        assert_eq!(status, 201, "{base}: {}", String::from_utf8_lossy(&reply));
        if base == RUST {
            assert_eq!(served_by.as_deref(), Some("rust"), "served by rust");
        }
        let value: serde_json::Value = serde_json::from_slice(&reply).unwrap();
        let infos = value["file_infos"].as_array().expect("two infos").clone();
        assert_eq!(infos.len(), 2, "{base}: two files");
        let image_id = infos[0]["id"].as_str().unwrap().to_owned();
        let files = derived(&client, base, &token, &image_id).await;
        seen.push((infos.into_iter().map(normalise).collect::<Vec<_>>(), files));
    }
    assert_eq!(seen[0].0, seen[1].0, "the FileInfo rows differ");
    assert_eq!(seen[0].1, seen[1].1, "the derived files differ");
}

/// The completing chunk of a resumable upload: `HandleImages` writes the thumbnail and preview
/// (no mini preview on this path), and both match Go.
#[tokio::test]
async fn the_uploads_route_completes_images_identically() {
    if !stack_enabled() {
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&client, &token).await;

    let mut with_derived = 0;
    for (label, filename, bytes) in corpus() {
        let mut seen = Vec::new();
        for base in [GO, RUST] {
            let session = serde_json::json!({
                "channel_id": channel,
                "filename": filename,
                "file_size": bytes.len(),
                "type": "attachment",
            });
            let (status, _, reply) = send(
                &client,
                base,
                reqwest::Method::POST,
                "/api/v4/uploads",
                &token,
                Some("application/json"),
                serde_json::to_vec(&session).unwrap(),
            )
            .await;
            assert_eq!(
                status,
                201,
                "{label} {base} createUpload: {}",
                String::from_utf8_lossy(&reply)
            );
            let session: serde_json::Value = serde_json::from_slice(&reply).unwrap();
            let upload_id = session["id"].as_str().unwrap().to_owned();

            let (status, served_by, reply) = send(
                &client,
                base,
                reqwest::Method::POST,
                &format!("/api/v4/uploads/{upload_id}"),
                &token,
                Some("application/octet-stream"),
                bytes.clone(),
            )
            .await;
            assert_eq!(
                status,
                200,
                "{label} {base} uploadData: {}",
                String::from_utf8_lossy(&reply)
            );
            if base == RUST {
                assert_eq!(
                    served_by.as_deref(),
                    Some("rust"),
                    "{label}: served by rust"
                );
            }
            let info: serde_json::Value = serde_json::from_slice(&reply).expect("a FileInfo");
            let file_id = info["id"].as_str().unwrap().to_owned();
            let files = derived(&client, base, &token, &file_id).await;
            let mut row = normalise(info);
            row["stored_paths"] = stored_paths(&file_id).await;
            seen.push((row, files));
        }
        assert_eq!(seen[0].0, seen[1].0, "{label}: the FileInfo rows differ");
        assert_eq!(seen[0].1, seen[1].1, "{label}: the derived files differ");
        if seen[1]
            .1
            .iter()
            .all(|(status, body)| *status == 200 && !body.is_empty())
        {
            with_derived += 1;
        }
    }
    assert_eq!(
        with_derived,
        corpus().len() - 3,
        "files with a thumbnail and a preview"
    );
}

/// A 1×1 black, uncompressed, little-endian, 8-bit grayscale TIFF: the smallest input that
/// reaches `golang.org/x/image/tiff`'s decoder and comes back with dimensions. Built here rather
/// than taken from a fixture because the TIFF oracle stage records what the *decoder* answers,
/// and this test is about the route forwarding before it ever gets that far.
fn tiny_tiff() -> Vec<u8> {
    // tag, type (3 = SHORT, 4 = LONG), value. Every entry has count 1, so the value sits inline.
    const PIXEL_AT: u32 = 8 + 2 + 9 * 12 + 4;
    let entries: [(u16, u16, u32); 9] = [
        (256, 3, 1),        // ImageWidth
        (257, 3, 1),        // ImageLength
        (258, 3, 8),        // BitsPerSample
        (259, 3, 1),        // Compression = none
        (262, 3, 1),        // PhotometricInterpretation = BlackIsZero
        (273, 4, PIXEL_AT), // StripOffsets
        (277, 3, 1),        // SamplesPerPixel
        (278, 4, 1),        // RowsPerStrip
        (279, 4, 1),        // StripByteCounts
    ];
    let mut out = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
    out.extend_from_slice(&u16::try_from(entries.len()).unwrap().to_le_bytes());
    for (tag, typ, value) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&typ.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        if typ == 3 {
            out.extend_from_slice(&u16::try_from(value).unwrap().to_le_bytes());
            out.extend_from_slice(&[0, 0]);
        } else {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(out.len(), PIXEL_AT as usize);
    out.push(0);
    out
}

/// The formats this port does not decode are still Go's, and forward before anything is
/// written. GIF and BMP are no longer among them — `goimage::gif` and `goimage::bmp` decode
/// them, and the derived files they produce are compared with everything else in `corpus()`.
#[tokio::test]
async fn the_undecoded_formats_still_forward() {
    if !stack_enabled() {
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&client, &token).await;

    let tiff = tiny_tiff();
    for (filename, bytes) in [("mmrs-parity-img.tiff", tiff.as_slice())] {
        let path = format!("/api/v4/files?channel_id={channel}&filename={filename}");
        let (status, served_by, body) = send(
            &client,
            RUST,
            reqwest::Method::POST,
            &path,
            &token,
            Some("application/octet-stream"),
            bytes.to_vec(),
        )
        .await;
        assert_eq!(
            status,
            201,
            "{filename}: {}",
            String::from_utf8_lossy(&body)
        );
        assert_ne!(
            served_by.as_deref(),
            Some("rust"),
            "{filename} must forward: this port does not decode it"
        );
        assert_eq!(first_info(&body)["width"], 1, "{filename}: Go measured it");
    }
}
