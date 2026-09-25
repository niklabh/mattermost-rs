//! Port of `app/users/profile_picture.go`: the generated initials avatar (`createProfileImage`)
//! and the three app functions around it — `GetDefaultProfileImage`, `GetProfileImage` and
//! `UpdateDefaultProfileImage`/`SetDefaultProfileImage`. The rasterisation is `gofont`'s
//! byte-exact port of golang/freetype; this module is the Mattermost call sequence over it.
//!
//! # The font is read, the bot image is embedded — as in Go
//!
//! Go reads `FileSettings.InitialFont` from the `fonts` directory `fileutils.FindDir` finds, on
//! every call, and so does this: the TTF is not in this repository. The bot's default image is a
//! `//go:embed` in Go (`bot_default_icon.png`), and is an `include_bytes!` of the same file here,
//! as the plugin signing keys are; `fixtures/behaviour_avatar.json` pins its SHA-256.

use gofont::draw::{Rgba64, draw_string, fill_src};
use gofont::face::{Face, Options};
use gofont::fixed::{Point26_6, i as fixed_i};
use gofont::truetype::Font;
use goimage::image::{Image, Pixels, Rect};
use goimage::png::writer::{CompressionLevel, encode};
use mm_model::user::User;
use mm_model::utils::{AppError, AppResult};
use mm_store::UserStore;

use crate::App;
use crate::post::PrepareError;

/// `botDefaultImage` (users/bot_default_image.go).
pub const BOT_DEFAULT_IMAGE: &[u8] = include_bytes!("images/bot_default_icon.png");

/// `imageProfilePixelDimension` (profile_picture.go:30).
const IMAGE_PROFILE_PIXEL_DIMENSION: i64 = 128;

/// The 26 background colours, in Go's order — repeats and all (profile_picture.go:106).
const COLORS: [(u8, u8, u8); 26] = [
    (197, 8, 126),
    (227, 207, 18),
    (28, 181, 105),
    (35, 188, 224),
    (116, 49, 196),
    (197, 8, 126),
    (197, 19, 19),
    (250, 134, 6),
    (227, 207, 18),
    (123, 201, 71),
    (28, 181, 105),
    (35, 188, 224),
    (116, 49, 196),
    (197, 8, 126),
    (197, 19, 19),
    (250, 134, 6),
    (227, 207, 18),
    (123, 201, 71),
    (28, 181, 105),
    (35, 188, 224),
    (116, 49, 196),
    (197, 8, 126),
    (197, 19, 19),
    (250, 134, 6),
    (227, 207, 18),
    (123, 201, 71),
];

/// Why `createProfileImage` failed — the three sentinel errors of users/errors.go that it can
/// return, plus the empty username, where Go indexes past the end of the string and panics.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileImageError {
    #[error("could not get default font")]
    DefaultFont,
    #[error("could not get glyph")]
    Glyph,
    #[error("could not encode image")]
    Encoding,
    /// Go: `strings.ToUpper(username)[0]` on `""` — an index panic. Not reachable through the
    /// API (a username is validated non-empty); mapped to `UserInitialsError`'s id rather than a
    /// panic.
    #[error("could not get user initials")]
    Initial,
}

/// FNV-1a, 32-bit (`hash/fnv.New32a`).
#[must_use]
pub fn fnv32a(data: &[u8]) -> u32 {
    let mut h: u32 = 2_166_136_261;
    for b in data {
        h ^= u32::from(*b);
        h = h.wrapping_mul(16_777_619);
    }
    h
}

/// `strings.ToUpper`, per rune with Go's **simple** case mapping: a rune whose full uppercase is
/// more than one rune (`ß` → `SS`) keeps itself, as `unicode.ToUpper` does.
fn go_to_upper(s: &str) -> String {
    s.chars()
        .map(|c| {
            let mut up = c.to_uppercase();
            match (up.next(), up.next()) {
                (Some(u), None) => u,
                _ => c,
            }
        })
        .collect()
}

/// The initial Go draws: `string(strings.ToUpper(username)[0])` — the first **byte** of the
/// uppercased name, converted to a rune. For a name starting with a multi-byte character that is
/// the Latin-1 rune of its lead byte (`é` → `Ã`), not the character.
fn initial(username: &str) -> Option<char> {
    go_to_upper(username)
        .as_bytes()
        .first()
        .map(|b| char::from(*b))
}

/// Port of `createProfileImage` (profile_picture.go:105) over an already-read font file.
pub fn create_profile_image(
    username: &str,
    user_id: &str,
    font_bytes: &[u8],
) -> Result<Vec<u8>, ProfileImageError> {
    let seed = fnv32a(user_id.as_bytes());
    let initial = initial(username).ok_or(ProfileImageError::Initial)?;
    let font = Font::parse(font_bytes).map_err(|_| ProfileImageError::DefaultFont)?;

    // `colors[int64(seed)%int64(len(colors))]`: the seed widened, so never negative.
    let (r, g, b) = COLORS[(i64::from(seed) % COLORS.len() as i64) as usize];

    let bounds = Rect::new(
        0,
        0,
        IMAGE_PROFILE_PIXEL_DIMENSION,
        IMAGE_PROFILE_PIXEL_DIMENSION,
    );
    let mut dst = Pixels::new(bounds, 4);
    fill_src(&mut dst, bounds, Rgba64::from_nrgba(r, g, b, 255));

    let mut face = Face::new(
        font,
        &Options {
            size: (IMAGE_PROFILE_PIXEL_DIMENSION / 2) as f64,
            ..Options::default()
        },
    )
    .map_err(|_| ProfileImageError::DefaultFont)?;

    let (glyph_bounds, advance) = face.glyph_bounds(initial).ok_or(ProfileImageError::Glyph)?;
    let dim = fixed_i(IMAGE_PROFILE_PIXEL_DIMENSION);
    let x = dim.wrapping_sub(advance) / 2;
    let y = dim.wrapping_add(glyph_bounds.max.y.wrapping_sub(glyph_bounds.min.y)) / 2;
    draw_string(
        &mut dst,
        Rgba64::WHITE,
        &mut face,
        Point26_6::new(x, y),
        &initial.to_string(),
    );

    let mut out = Vec::new();
    encode(
        &mut out,
        &Image::Rgba(dst),
        CompressionLevel::BestCompression,
    )
    .map_err(|_| ProfileImageError::Encoding)?;
    Ok(out)
}

/// `getFont`'s file name: the old default `luximbi.ttf` is read as the new one.
fn font_file_name(initial_font: &str) -> &str {
    if initial_font == "luximbi.ttf" {
        "nunito-bold.ttf"
    } else {
        initial_font
    }
}

/// Port of `Server.GetDefaultProfileImage`'s error mapping (app/server.go:1914).
fn default_profile_image_error(err: &ProfileImageError) -> Box<AppError> {
    let id = match err {
        ProfileImageError::DefaultFont => "api.user.create_profile_image.default_font.app_error",
        ProfileImageError::Initial => "api.user.create_profile_image.initial.app_error",
        ProfileImageError::Glyph | ProfileImageError::Encoding => {
            "api.user.create_profile_image.encode.app_error"
        }
    };
    AppError::boxed("GetDefaultProfileImage", id, None, err.to_string(), 500)
}

impl App {
    /// Port of `Server.GetDefaultProfileImage` over `UserService.GetDefaultProfileImage`
    /// (users/profile_picture.go:95): a bot's embedded image, anyone else's initials avatar in
    /// `FileSettings.InitialFont` from the `fonts` directory.
    pub async fn get_default_profile_image(&self, user: &User) -> AppResult<Vec<u8>> {
        if user.is_bot {
            return Ok(BOT_DEFAULT_IMAGE.to_vec());
        }
        let config = self.config();
        let (dir, _) = crate::logs::find_dir("fonts");
        let path = dir.join(font_file_name(&config.initial_font));
        let font_bytes = tokio::fs::read(&path).await.map_err(|err| {
            tracing::warn!(path = %path.display(), error = %err, "the avatar font does not read");
            default_profile_image_error(&ProfileImageError::DefaultFont)
        })?;
        let username = user.username.clone();
        let user_id = user.id.clone();
        // A few milliseconds of rasterising and deflating: off the async executor.
        tokio::task::spawn_blocking(move || create_profile_image(&username, &user_id, &font_bytes))
            .await
            .map_err(|err| {
                AppError::boxed(
                    "GetDefaultProfileImage",
                    "api.user.create_profile_image.encode.app_error",
                    None,
                    err.to_string(),
                    500,
                )
            })?
            .map_err(|err| default_profile_image_error(&err))
    }

    /// Port of `App.UpdateDefaultProfileImage` (app/user.go:1027): generate, write to
    /// `users/<id>/profile.png`, reset `LastPictureUpdate` (a failure there only logged), and
    /// clear what Go would drop from its user cache.
    pub async fn update_default_profile_image(&self, user: &User) -> Result<(), PrepareError> {
        let image = self.get_default_profile_image(user).await?;
        self.write_file(&image, &crate::user::profile_image_path(&user.id))
            .await?;
        if let Err(err) = self
            .store()
            .user()
            .reset_last_picture_update(&user.id)
            .await
        {
            tracing::warn!(error = %err, "Failed to reset last picture update");
        }
        self.invalidate_cache_for_user(&user.id).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv32a_is_gos() {
        // `hash/fnv` of the empty input is the offset basis; "a" is a published test vector.
        assert_eq!(fnv32a(b""), 0x811c_9dc5);
        assert_eq!(fnv32a(b"a"), 0xe40c_292c);
    }

    /// Go takes the first **byte**: a name starting with `é` draws `Ã`, and `ß` stays `ß` because
    /// its uppercase is two letters.
    #[test]
    fn the_initial_is_the_first_byte_of_the_uppercased_name() {
        assert_eq!(initial("alice"), Some('A'));
        assert_eq!(initial("0day"), Some('0'));
        assert_eq!(initial("élan"), Some('Ã'));
        assert_eq!(initial("ßharp"), Some('Ã'));
        assert_eq!(initial(""), None);
    }

    #[test]
    fn the_old_default_font_is_read_as_the_new_one() {
        assert_eq!(font_file_name("luximbi.ttf"), "nunito-bold.ttf");
        assert_eq!(font_file_name("other.ttf"), "other.ttf");
    }
}

#[cfg(test)]
mod go_parity {
    //! Replays `fixtures/behaviour_avatar.json`'s `avatars` — `UserService.GetDefaultProfileImage`
    //! itself — byte for byte.

    use super::*;
    use sha2::Digest;

    #[test]
    fn every_avatar_is_gos_png_byte_for_byte() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/behaviour_avatar.json"
        );
        let oracle: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("oracle")).expect("JSON");
        let font = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../reference/mattermost/server/fonts/nunito-bold.ttf"
        ))
        .expect("the reference tree's font");

        let mut failures = Vec::new();
        let cases = oracle["avatars"].as_array().expect("avatars");
        for case in cases {
            let username = case["username"].as_str().expect("username");
            let user_id = case["user_id"].as_str().expect("user_id");
            assert_eq!(
                i64::from(fnv32a(user_id.as_bytes())),
                case["seed"],
                "{user_id}: the seed"
            );
            let got = if case["is_bot"] == true {
                Ok(BOT_DEFAULT_IMAGE.to_vec())
            } else {
                create_profile_image(username, user_id, &font)
            };
            match (got, case.get("error")) {
                (Err(_), Some(_)) => {}
                (Ok(png), None) => {
                    let sha: String = sha2::Sha256::digest(&png)
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    if sha != case["png_sha256"] || png.len() as i64 != case["png_len"] {
                        failures.push(format!(
                            "{username:?} ({user_id}): {} bytes vs {}",
                            png.len(),
                            case["png_len"]
                        ));
                    }
                }
                (got, err) => failures.push(format!(
                    "{username:?}: {:?} vs Go's {err:?}",
                    got.map(|p| p.len())
                )),
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {} avatars differ from Go:\n{}",
            failures.len(),
            cases.len(),
            failures.join("\n")
        );
    }
}
