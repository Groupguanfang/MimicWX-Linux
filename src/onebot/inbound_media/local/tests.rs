use super::*;
use std::os::unix::fs::symlink;

const GIF: &str =
    "47494638396101000100800000000000ffffff21f90401000000002c000000000100010000020144003b";
const DAT: &str = "0708563208072000000004000000017f43b5063143943e5a6fc45ae6b048f1d930cff666493bfe0d912b178fd91ac6ab691dcd89365db5d946be48719f017e0100010000024104407b";
const HASH: &str = "0123456789abcdef0123456789abcdef";

fn from_hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

struct Fixture {
    root: PathBuf,
    account: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).unwrap();
        let root =
            std::env::temp_dir().join(format!("mimicwx-images-{}", u64::from_le_bytes(random)));
        let account = root.join("xwechat_files/wxid_fixture_1234");
        std::fs::create_dir_all(&account).unwrap();
        let kvcomm = root.join(".xwechat/net/kvcomm");
        std::fs::create_dir_all(&kvcomm).unwrap();
        std::fs::write(kvcomm.join("key_123456_0.statistic"), []).unwrap();
        Self { root, account }
    }

    fn metadata(&self) -> LocalImageMetadata {
        LocalImageMetadata {
            account: self.account.clone(),
            wxid: "wxid_fixture".into(),
            chat: "44465171684@chatroom".into(),
            create_time: 1794883218,
            hashes: vec![HASH.into()],
            content: String::new(),
        }
    }

    fn write(&self, thumbnail: bool, bytes: &[u8]) -> PathBuf {
        let path = paths(&self.metadata(), false)
            .into_iter()
            .find(|(_, quality)| (*quality == ImageQuality::Thumbnail) == thumbnail)
            .unwrap()
            .0;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn decrypts_independent_openssl_v2_fixture_and_rejects_corrupt_segments() {
    let bytes = from_hex(DAT);
    let key = (*b"e8509b295d88faf7", 64);
    assert_eq!(decode_dat(&bytes, Some(key)).unwrap(), from_hex(GIF));
    assert!(decode_dat(&bytes, None).is_err());
    assert!(decode_dat(&bytes, Some(([0; 16], 64))).is_err());
    assert!(decode_dat(&bytes[..45], Some(key)).is_err());
    let mut corrupt = bytes.clone();
    corrupt[6..10].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(decode_dat(&corrupt, Some(key)).is_err());
    let mut corrupt = bytes.clone();
    corrupt[10..14].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(decode_dat(&corrupt, Some(key)).is_err());
    let legacy = from_hex(GIF)
        .into_iter()
        .map(|byte| byte ^ 0x88)
        .collect::<Vec<_>>();
    assert_eq!(decode_dat(&legacy, None).unwrap(), from_hex(GIF));
}

#[tokio::test]
async fn derives_account_key_reads_original_and_marks_thumbnail_fallback() {
    let fixture = Fixture::new();
    let original = fixture.write(false, &from_hex(DAT));
    fixture.write(true, &from_hex(DAT));
    let result = image(fixture.metadata(), None, false, true).await.unwrap();
    assert_eq!(result.bytes.as_ref(), from_hex(GIF));
    assert_eq!(result.content_type, "image/gif");
    assert_eq!(result.quality, ImageQuality::Original);
    std::fs::write(&original, b"corrupt").unwrap();
    let result = image(fixture.metadata(), None, false, false).await.unwrap();
    assert_eq!(result.quality, ImageQuality::Thumbnail);
    let kvcomm = fixture.root.join(".xwechat/net/kvcomm");
    std::fs::rename(
        kvcomm.join("key_123456_0.statistic"),
        kvcomm.join("transport_123456_0.statistic"),
    )
    .unwrap();
    assert_eq!(
        image(fixture.metadata(), None, false, false)
            .await
            .unwrap()
            .quality,
        ImageQuality::Thumbnail
    );
    let mut another_group = fixture.metadata();
    another_group.chat = "another@chatroom".into();
    assert!(image(another_group, None, false, false).await.is_err());
}

#[tokio::test]
async fn waits_for_a_local_image_to_finish_arriving() {
    let fixture = Fixture::new();
    let metadata = fixture.metadata();
    let pending = tokio::spawn(image(metadata, None, false, true));
    tokio::time::sleep(Duration::from_millis(150)).await;
    fixture.write(false, &[]);
    tokio::time::sleep(Duration::from_millis(150)).await;
    fixture.write(false, &from_hex(DAT)[..31]);
    tokio::time::sleep(Duration::from_millis(250)).await;
    fixture.write(false, &from_hex(DAT));
    let result = pending.await.unwrap().unwrap();
    assert_eq!(result.bytes.as_ref(), from_hex(GIF));
    assert_eq!(result.quality, ImageQuality::Original);
}

#[tokio::test]
async fn only_accepts_an_intermediate_file_if_it_matches_the_original_checksum() {
    let fixture = Fixture::new();
    let preview = paths(&fixture.metadata(), false)
        .into_iter()
        .find(|(_, quality)| *quality == ImageQuality::Preview)
        .unwrap()
        .0;
    let plain = from_hex(GIF);
    std::fs::create_dir_all(preview.parent().unwrap()).unwrap();
    std::fs::write(&preview, &plain).unwrap();
    assert_eq!(
        image(fixture.metadata(), None, false, false)
            .await
            .unwrap()
            .quality,
        ImageQuality::Preview
    );
    let checksum = format!("{:x}", md5::compute(&plain));
    assert_eq!(
        image(fixture.metadata(), Some(&checksum), false, true)
            .await
            .unwrap()
            .bytes
            .as_ref(),
        plain
    );
    let original = [plain.as_slice(), b"original"].concat();
    fixture.write(false, &original);
    assert_eq!(
        image(fixture.metadata(), Some(&checksum), false, true)
            .await
            .unwrap()
            .bytes
            .as_ref(),
        original
    );
}

#[tokio::test]
async fn missing_fallback_files_do_not_hide_the_original_decode_error() {
    let fixture = Fixture::new();
    fixture.write(false, b"unsupported");
    assert_eq!(
        image(fixture.metadata(), None, false, false)
            .await
            .err()
            .as_deref(),
        Some("不支持的本地图片格式")
    );
}

#[tokio::test]
async fn recovers_image_keys_after_wechat_removes_statistics_and_reuses_them_for_gifs() {
    let fixture = Fixture::new();
    std::fs::remove_file(
        fixture
            .root
            .join(".xwechat/net/kvcomm/key_123456_0.statistic"),
    )
    .unwrap();
    let mut metadata = fixture.metadata();
    metadata.account = fixture.root.join("xwechat_files/wxid_fixture_e10a");
    let original = paths(&metadata, false)[0].0.clone();
    std::fs::create_dir_all(original.parent().unwrap()).unwrap();
    let encoded = from_hex("070856320807200000000200000001d5989875b4f829b6a10a48e5ff098ac18909c874dbdb9aedb67e529b63222e17ab691dcd89365db5d946be48719f017ebf99");
    std::fs::write(&original, &encoded).unwrap();
    let result = image(
        LocalImageMetadata {
            account: metadata.account.clone(),
            wxid: metadata.wxid.clone(),
            chat: metadata.chat.clone(),
            create_time: metadata.create_time,
            hashes: metadata.hashes.clone(),
            content: String::new(),
        },
        None,
        false,
        true,
    )
    .await
    .unwrap();
    assert_eq!(result.content_type, "image/jpeg");
    let mut expected = vec![0xff, 0xd8, 0xff];
    expected.extend_from_slice(&[0; 29]);
    expected.extend_from_slice(&[0xff, 0xd9]);
    assert_eq!(result.bytes.as_ref(), expected);
    assert_eq!(result.quality, ImageQuality::Original);
    assert!(recover_key(&encoded, &metadata.account, "wxid_another").is_none());
    assert!(recover_key(&encoded[..30], &metadata.account, &metadata.wxid).is_none());
    std::fs::write(&original, from_hex(DAT)).unwrap();
    let result = image(metadata, None, false, true).await.unwrap();
    assert_eq!(result.content_type, "image/gif");
    assert_eq!(result.bytes.as_ref(), from_hex(GIF));
}

#[test]
fn refuses_other_account_files_symlink_escape_and_invalid_hashes() {
    let fixture = Fixture::new();
    let outside = fixture.root.join("outside.gif");
    std::fs::write(&outside, from_hex(GIF)).unwrap();
    let link = fixture.account.join("escape.dat");
    symlink(&outside, &link).unwrap();
    assert!(read_image(&link, &fixture.account).is_err());
    let mut metadata = fixture.metadata();
    metadata.hashes = vec!["../../outside".into()];
    assert!(paths(&metadata, false).is_empty());
    metadata.create_time = -1;
    assert!(paths(&metadata, false).is_empty());
    assert!(decode_dat(&vec![0; MAX_BYTES + 1], None).is_err());
}

#[tokio::test]
async fn converts_wxgf_hevc_to_png_and_rejects_invalid_payloads() {
    let encoded = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-threads",
            "1",
            "-filter_threads",
            "1",
            "-filter_complex_threads",
            "1",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=32x32",
            "-frames:v",
            "1",
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "pools=none:frame-threads=1:log-level=error",
            "-f",
            "hevc",
            "pipe:1",
        ])
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        encoded.status.success(),
        "{}",
        String::from_utf8_lossy(&encoded.stderr)
    );
    let mut wxgf = b"wxgf".to_vec();
    wxgf.extend_from_slice(&[0; 33]);
    wxgf.extend_from_slice(&encoded.stdout);
    let decoded = wxgf_to_png(&wxgf).await.unwrap();
    assert_eq!(media::image_content_type(&decoded), Some("image/png"));
    assert_eq!(u32::from_be_bytes(decoded[16..20].try_into().unwrap()), 32);
    assert_eq!(u32::from_be_bytes(decoded[20..24].try_into().unwrap()), 32);
    assert!(wxgf_to_png(b"wxgf-invalid").await.is_err());
}
