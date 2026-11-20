//! Apple iCloud Photos, read-only metadata of the personal library: albums, photos, search,
//! one photo, and an explicit refresh that reads Apple's change feed into the cache.
//! Every fixture is synthetic and every remote is a loopback mock.
use base64::Engine;
use quince_auth::{session::Session, Client, Endpoints, Store};
use quince_core::photos::{Album, Kind, Photo};
use serde_json::{json, Value};
use skykeep_mcp::Server;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const DSID: &str = "1234567890";
const DB: &str = "/database/1/com.apple.photos.cloud/production/private";

fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn photo(id: &str, taken_at: i64) -> Photo {
    Photo {
        id: id.into(),
        master: format!("M-{id}"),
        taken_at,
        added_at: Some(taken_at + 60),
        kind: Kind::Image,
        width: 4032,
        height: 3024,
        duration: None,
        filename: Some(format!("{id}.HEIC")),
        size: Some(2_000_000),
        uti: Some("public.heic".into()),
        favorite: false,
        hidden: false,
        deleted: false,
        deleted_at: None,
        expunge_at: None,
        title: None,
        caption: None,
        rating: 0,
        keywords: Vec::new(),
        latitude: None,
        longitude: None,
        rendered: 0,
        subtype: 0,
        burst: None,
        burst_pick: false,
        burst_count: 0,
        live_size: None,
        change_tag: Some("tag".into()),
    }
}

/// A personal library with a Live Photo, a video, a hidden and a deleted photo, and a photo
/// of a Shared Library. The cache is given a place for one photo, as another client could have
/// stored it: no reply may show it.
fn seed(library: &mut quince_core::Library) {
    let mut beach = photo("P-BEACH", 1_700_000_300);
    beach.title = Some("Beach day".into());
    beach.caption = Some("Sand and SYNTHETIC waves".into());
    beach.keywords = vec!["summer".into(), "sea".into()];
    beach.favorite = true;
    beach.rating = 4;
    beach.latitude = Some(52.123_456);
    beach.longitude = Some(13.654_321);
    let mut live = photo("P-LIVE", 1_700_000_200);
    live.kind = Kind::Live;
    live.live_size = Some(1_500_000);
    let mut video = photo("P-VIDEO", 1_700_000_100);
    video.kind = Kind::Video;
    video.uti = Some("com.apple.quicktime-movie".into());
    video.duration = Some(12.917);
    video.filename = Some("CLIP.MOV".into());
    let mut hidden = photo("P-HIDDEN", 1_700_000_400);
    hidden.hidden = true;
    hidden.title = Some("Beach secret".into());
    let mut deleted = photo("P-DELETED", 1_700_000_500);
    deleted.deleted = true;
    deleted.title = Some("Beach gone".into());
    library
        .put_photos_in(None, &[beach, live, video, hidden, deleted])
        .unwrap();
    let mut shared = photo("P-SHARED", 1_700_000_600);
    shared.title = Some("Beach shared".into());
    library
        .put_photos_in(Some("SharedSync-SYNTHETIC"), &[shared])
        .unwrap();
    library
        .replace_albums(&[
            Album {
                id: "ALB-TRIP".into(),
                name: "Trip".into(),
                position: 1,
                change_tag: None,
                folder: false,
                parent: Some("FOLDER-1".into()),
            },
            Album {
                id: "FOLDER-1".into(),
                name: "Holidays".into(),
                position: 0,
                change_tag: None,
                folder: true,
                parent: None,
            },
        ])
        .unwrap();
    library
        .put_album_items(&[
            ("ALB-TRIP".into(), "P-VIDEO".into(), 0),
            ("ALB-TRIP".into(), "P-BEACH".into(), 1),
            ("ALB-TRIP".into(), "P-HIDDEN".into(), 2),
            ("ALB-TRIP".into(), "P-SHARED".into(), 3),
        ])
        .unwrap();
}

fn bound(policy: Option<&str>) -> (tempfile::TempDir, Server) {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library
        .set_state("account", &quince_sync::queue::account_tag(DSID))
        .unwrap();
    if let Some(mode) = policy {
        server.set_policy(mode).unwrap();
    }
    (dir, server)
}

fn seeded(policy: Option<&str>) -> (tempfile::TempDir, Server) {
    let (dir, server) = bound(policy);
    seed(&mut quince_core::Library::open(dir.path()).unwrap());
    (dir, server)
}

async fn init(server: &mut Server, forms: bool) {
    let capabilities = if forms {
        json!({"elicitation":{"form":{}}})
    } else {
        json!({})
    };
    server
        .handle(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":capabilities}}))
        .await
        .unwrap();
}

fn text(reply: &Value) -> Value {
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn reply_bytes(reply: &Value) -> usize {
    reply["result"]["content"][0]["text"]
        .as_str()
        .map(|t| serde_json::to_string(t).unwrap().len())
        .unwrap()
}

async fn call(server: &mut Server, id: i64, name: &str, args: Value) -> Value {
    server
        .handle(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}}))
        .await
        .unwrap()
}

async fn ok(server: &mut Server, name: &str, args: Value) -> Value {
    let reply = call(server, 7, name, args).await;
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    text(&reply)
}

async fn error(server: &mut Server, name: &str, args: Value) -> String {
    let reply = call(server, 7, name, args).await;
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    text(&reply)["error"].as_str().unwrap().to_string()
}

fn ids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap_or("<unsupported>").to_string())
        .collect()
}

/// No reply ever carries a place, a face, a download link or an excluded photo.
fn nothing_private(value: &Value) {
    let text = value.to_string();
    for word in [
        "latitude",
        "longitude",
        "52.12",
        "13.65",
        "downloadURL",
        "synthetic.invalid",
        "SYNTHETIC-PERSON",
        "P-HIDDEN",
        "P-DELETED",
        "P-SHARED",
        "secret",
        "Beach gone",
        "Beach shared",
    ] {
        assert!(!text.contains(word), "{word} in {text}");
    }
}

const NAMES: [&str; 5] = [
    "photos_albums",
    "photos_list",
    "photos_search",
    "photos_get",
    "photos_refresh",
];

#[tokio::test]
async fn tools_list_names_apple_icloud_photos_with_strict_read_only_schemas() {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    let init = server
        .handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{}}}))
        .await
        .unwrap();
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(
        instructions.contains("Apple iCloud Photos"),
        "{instructions}"
    );
    assert!(instructions.contains("photos_refresh"), "{instructions}");
    let list = server
        .handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .await
        .unwrap();
    let tools = list["result"]["tools"].as_array().unwrap();
    let photos: Vec<&Value> = tools
        .iter()
        .filter(|t| t["name"].as_str().unwrap().starts_with("photos"))
        .collect();
    let names: Vec<&str> = photos.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, NAMES);
    for tool in &photos {
        let name = tool["name"].as_str().unwrap();
        let description = tool["description"].as_str().unwrap();
        assert!(description.contains("Apple iCloud Photos"), "{name}");
        assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false, "{name}");
        let hints = &tool["annotations"];
        assert_eq!(hints["destructiveHint"], false, "{name}");
        if name == "photos_refresh" {
            assert_eq!(hints["readOnlyHint"], false, "refresh replaces the cache");
            assert_eq!(hints["openWorldHint"], true, "refresh reads from Apple");
            assert_eq!(tool["inputSchema"]["properties"], json!({}));
        } else {
            assert_eq!(hints["readOnlyHint"], true, "{name}");
            assert_eq!(hints["openWorldHint"], false, "{name}");
        }
        for word in ["write", "delete", "download", "upload", "move"] {
            assert!(!name.contains(word), "{name}");
        }
    }
    assert_eq!(
        photos[1]["inputSchema"]["properties"]["limit"]["maximum"],
        100
    );
    assert_eq!(
        photos[1]["inputSchema"]["properties"]["album_id"]["maxLength"],
        512
    );
    assert_eq!(photos[2]["inputSchema"]["required"], json!(["query"]));
    assert_eq!(
        photos[2]["inputSchema"]["properties"]["query"]["maxLength"],
        256
    );
    assert_eq!(photos[3]["inputSchema"]["required"], json!(["id"]));
}

#[tokio::test]
async fn list_shows_only_visible_personal_photos_without_places() {
    let (_dir, mut server) = seeded(None);
    init(&mut server, false).await;
    let page = ok(&mut server, "photos_list", json!({})).await;
    assert_eq!(ids(&page), ["P-BEACH", "P-LIVE", "P-VIDEO"]);
    assert_eq!(page["source"], "local_cache");
    assert_eq!(page["library"], "personal");
    assert_eq!(page["limit"], 25);
    assert_eq!(page["remote_verified"], false);
    let items = page["items"].as_array().unwrap();
    assert_eq!(items[0]["kind"], "image");
    assert_eq!(items[0]["title"], "Beach day");
    assert_eq!(items[0]["favorite"], true);
    assert_eq!(items[0]["rating"], 4);
    assert_eq!(items[0]["keywords"], json!(["summer", "sea"]));
    assert_eq!(items[0]["taken_at"], "2023-11-14T22:18:20Z");
    assert_eq!(items[0]["shortened"], json!([]));
    assert_eq!(items[1]["kind"], "live_photo");
    assert_eq!(items[1]["live_photo_video_bytes"], 1_500_000);
    assert_eq!(items[2]["kind"], "video");
    assert_eq!(items[2]["duration_seconds"], 12.917);
    assert_eq!(items[2]["filename"], "CLIP.MOV");
    nothing_private(&page);
    // Every field is one of these: an allowlist, never the cache's whole row.
    let allowed = [
        "id",
        "kind",
        "taken_at",
        "added_at",
        "width",
        "height",
        "duration_seconds",
        "filename",
        "title",
        "caption",
        "keywords",
        "keywords_total",
        "favorite",
        "rating",
        "file_type",
        "size_bytes",
        "live_photo_video_bytes",
        "shortened",
    ];
    for item in items {
        for key in item.as_object().unwrap().keys() {
            assert!(allowed.contains(&key.as_str()), "{key}");
        }
    }
}

#[tokio::test]
async fn albums_list_and_album_pages_leave_out_hidden_and_shared_photos() {
    let (_dir, mut server) = seeded(Some("writes_only"));
    init(&mut server, false).await;
    let albums = ok(&mut server, "photos_albums", json!({})).await;
    assert_eq!(albums["total"], 2);
    assert_eq!(albums["album_index_capped"], false);
    let items = albums["items"].as_array().unwrap();
    assert_eq!(items[0]["id"], "FOLDER-1");
    assert_eq!(items[0]["folder"], true);
    assert_eq!(items[1]["id"], "ALB-TRIP");
    assert_eq!(items[1]["name"], "Trip");
    assert_eq!(items[1]["parent_id"], "FOLDER-1");
    // Counted as the album's page shows it: the hidden and the Shared Library photo left out.
    assert_eq!(items[1]["photo_count"], 2);
    assert_eq!(items[0]["photo_count"], 0);
    let page = ok(&mut server, "photos_list", json!({"album_id":"ALB-TRIP"})).await;
    assert_eq!(ids(&page), ["P-VIDEO", "P-BEACH"], "album order");
    assert_eq!(page["album"]["name"], "Trip");
    nothing_private(&page);
    nothing_private(&albums);
    assert_eq!(
        error(&mut server, "photos_list", json!({"album_id":"NOPE"})).await,
        "album_not_found"
    );
    assert_eq!(
        error(&mut server, "photos_list", json!({"album_id":"FOLDER-1"})).await,
        "album_is_folder"
    );
}

#[tokio::test]
async fn search_and_get_read_one_photo_and_say_what_is_not_kept() {
    let (_dir, mut server) = seeded(Some("yolo"));
    init(&mut server, false).await;
    let found = ok(&mut server, "photos_search", json!({"query":"beach"})).await;
    assert_eq!(ids(&found), ["P-BEACH"]);
    nothing_private(&found);
    let one = ok(&mut server, "photos_get", json!({"id":"P-BEACH"})).await;
    let p = &one["photo"];
    assert_eq!(p["caption"], "Sand and SYNTHETIC waves");
    assert_eq!(p["file_type"], "public.heic");
    assert_eq!(p["size_bytes"], 2_000_000);
    assert_eq!(one["photo_shortened"], false);
    let not = one["not_included"].to_string();
    for word in ["location", "faces", "thumbnail", "Shared Library", "hidden"] {
        assert!(not.contains(word), "{word}: {not}");
    }
    nothing_private(&one["photo"]);
    assert!(!one["photo"].to_string().contains("52.12"));
    for hidden in ["P-HIDDEN", "P-DELETED", "P-SHARED", "P-NONE"] {
        assert_eq!(
            error(&mut server, "photos_get", json!({"id":hidden})).await,
            "not_found",
            "{hidden}"
        );
    }
    // Excluded photos are not found by their words either.
    for query in ["secret", "gone", "shared"] {
        let page = ok(&mut server, "photos_search", json!({"query":query})).await;
        assert!(ids(&page).is_empty(), "{query}: {page}");
    }
}

#[tokio::test]
async fn arguments_are_bounded_and_checked_before_any_read() {
    let (_dir, mut server) = seeded(None);
    init(&mut server, false).await;
    for (name, args, code) in [
        ("photos_list", json!({"limit":0}), "invalid_arguments"),
        ("photos_list", json!({"limit":101}), "invalid_arguments"),
        ("photos_list", json!({"album_id":""}), "invalid_arguments"),
        (
            "photos_list",
            json!({"album_id":"a".repeat(513)}),
            "invalid_arguments",
        ),
        ("photos_list", json!({"extra":1}), "invalid_arguments"),
        ("photos_albums", json!({"limit":101}), "invalid_arguments"),
        ("photos_search", json!({"query":"  "}), "query_required"),
        (
            "photos_search",
            json!({"query":"é".repeat(129)}),
            "query_too_long",
        ),
        (
            "photos_get",
            json!({"id":"a".repeat(513)}),
            "invalid_arguments",
        ),
        ("photos_get", json!({}), "invalid_arguments"),
        ("photos_refresh", json!({"full":true}), "invalid_arguments"),
    ] {
        assert_eq!(
            error(&mut server, name, args.clone()).await,
            code,
            "{name} {args}"
        );
    }
    // 256 bytes exactly is allowed.
    ok(
        &mut server,
        "photos_search",
        json!({"query":"é".repeat(128)}),
    )
    .await;
    ok(&mut server, "photos_list", json!({"limit":100})).await;
}

#[tokio::test]
async fn pages_are_stable_and_follow_their_offsets() {
    let (dir, mut server) = bound(None);
    let photos: Vec<Photo> = (0..130)
        .map(|n| photo(&format!("P-{n:03}"), 1_700_000_000 + (n / 2)))
        .collect();
    quince_core::Library::open(dir.path())
        .unwrap()
        .put_photos_in(None, &photos)
        .unwrap();
    init(&mut server, false).await;
    let first = ok(&mut server, "photos_list", json!({})).await;
    assert_eq!(first["items"].as_array().unwrap().len(), 25);
    assert_eq!(first["total"], 130);
    assert_eq!(first["next_offset"], 25);
    let mut seen = Vec::new();
    let mut offset = 0;
    loop {
        let page = ok(
            &mut server,
            "photos_list",
            json!({"offset":offset,"limit":100}),
        )
        .await;
        seen.extend(ids(&page));
        match page["next_offset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert_eq!(seen.len(), 130);
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 130, "no photo twice");
    // Newest first, and a tie on the time goes by id.
    assert_eq!(&seen[..2], ["P-128", "P-129"]);
    let again = ok(&mut server, "photos_list", json!({})).await;
    assert_eq!(ids(&again), ids(&first));
    let past = ok(&mut server, "photos_list", json!({"offset":500})).await;
    assert!(ids(&past).is_empty());
    assert!(past["next_offset"].is_null());
}

#[tokio::test]
async fn long_fields_are_shortened_marked_and_fit_their_caps() {
    let (dir, mut server) = bound(None);
    let mut long = photo("P-LONG", 1_700_000_000);
    long.title = Some("t".repeat(300));
    long.filename = Some("f".repeat(300));
    long.caption = Some("c".repeat(3000));
    long.keywords = (0..60)
        .map(|n| format!("{n:02}{}", "k".repeat(98)))
        .collect();
    // Control characters take six bytes each once encoded, twice over.
    let mut huge = photo(&"\u{1}".repeat(500), 1_699_000_000);
    huge.title = Some("\u{1}".repeat(300));
    huge.filename = Some("\u{1}".repeat(300));
    huge.caption = Some("\u{1}".repeat(5000));
    huge.keywords = (0..60).map(|_| "\u{1}".repeat(100)).collect();
    let too_long = photo(&"x".repeat(600), 1_698_000_000);
    let mut many: Vec<Photo> = vec![long, huge, too_long];
    for n in 0..97 {
        let mut p = photo(&format!("P-HUGE-{n:02}"), 1_690_000_000 - n);
        p.title = Some("\u{1}".repeat(300));
        p.filename = Some("\u{1}".repeat(300));
        p.caption = Some("\u{1}".repeat(300));
        p.keywords = (0..60).map(|_| "\u{1}".repeat(100)).collect();
        many.push(p);
    }
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library.put_photos_in(None, &many).unwrap();
    library
        .replace_albums(&[Album {
            id: "ALB-LONG".into(),
            name: "n".repeat(300),
            position: 0,
            change_tag: None,
            folder: false,
            parent: None,
        }])
        .unwrap();
    init(&mut server, false).await;
    let reply = call(&mut server, 9, "photos_list", json!({"limit":100})).await;
    assert!(reply_bytes(&reply) <= 262_144, "{}", reply_bytes(&reply));
    let page = text(&reply);
    let items = page["items"].as_array().unwrap();
    let first = &items[0];
    assert_eq!(first["id"], "P-LONG");
    assert_eq!(first["title"].as_str().unwrap().chars().count(), 256);
    assert_eq!(first["filename"].as_str().unwrap().chars().count(), 256);
    assert_eq!(first["caption"].as_str().unwrap().chars().count(), 256);
    assert_eq!(first["keywords"].as_array().unwrap().len(), 50);
    assert_eq!(first["keywords_total"], 60);
    assert_eq!(first["keywords"][0].as_str().unwrap().chars().count(), 64);
    assert_eq!(
        first["shortened"],
        json!(["filename", "title", "caption", "keywords"])
    );
    // The identifier is never shortened, whatever else is.
    assert_eq!(items[1]["id"], "\u{1}".repeat(500));
    assert!(
        serde_json::to_string(&serde_json::to_string(&items[1]).unwrap())
            .unwrap()
            .len()
            <= 8192
    );
    // One too long to ask for is held in place, without a cut identifier.
    assert!(items[2]["id"].is_null());
    assert_eq!(items[2]["unsupported"], "id_too_long");
    assert_eq!(items[2]["id_bytes"], 600);
    assert_eq!(page["unsupported_items"], 1);
    for item in items {
        let size = serde_json::to_string(&serde_json::to_string(item).unwrap())
            .unwrap()
            .len();
        assert!(size <= 8192, "{size}");
    }
    if page["page_shortened_to_fit"] == true {
        assert!(items.len() < 100);
        assert_eq!(page["next_offset"], items.len());
    }
    // In detail, a caption keeps up to 2,048 characters.
    let one = ok(&mut server, "photos_get", json!({"id":"P-LONG"})).await;
    assert_eq!(
        one["photo"]["caption"].as_str().unwrap().chars().count(),
        2048
    );
    assert_eq!(one["photo_shortened"], true);
    assert!(one["photo"]["shortened"]
        .as_array()
        .unwrap()
        .contains(&json!("caption")));
    let reply = call(
        &mut server,
        10,
        "photos_get",
        json!({"id":"\u{1}".repeat(500)}),
    )
    .await;
    assert!(reply_bytes(&reply) <= 65_536, "{}", reply_bytes(&reply));
    assert_eq!(text(&reply)["photo"]["id"], "\u{1}".repeat(500));
    let albums = ok(&mut server, "photos_albums", json!({})).await;
    let album = &albums["items"][0];
    assert_eq!(album["name"].as_str().unwrap().chars().count(), 256);
    assert_eq!(album["shortened"], json!(["name"]));
}

#[tokio::test]
async fn a_huge_file_type_is_shortened_and_pages_still_move_on() {
    let (dir, mut server) = bound(None);
    let odd: Vec<Photo> = (0..100)
        .map(|n| {
            let mut p = photo(&format!("P-ODD-{n:02}"), 1_700_000_000 - n);
            p.uti = Some("\u{1}".repeat(50_000));
            p
        })
        .collect();
    quince_core::Library::open(dir.path())
        .unwrap()
        .put_photos_in(None, &odd)
        .unwrap();
    init(&mut server, false).await;
    let reply = call(&mut server, 3, "photos_list", json!({"limit":100})).await;
    assert!(reply_bytes(&reply) <= 262_144);
    let page = text(&reply);
    let items = page["items"].as_array().unwrap();
    assert!(!items.is_empty(), "a page always makes progress: {page}");
    assert_eq!(
        page["next_offset"].as_u64(),
        (items.len() < 100).then_some(items.len() as u64)
    );
    for item in items {
        let size = serde_json::to_string(&serde_json::to_string(item).unwrap())
            .unwrap()
            .len();
        assert!(size <= 8192, "{size}");
        assert!(item["shortened"]
            .as_array()
            .unwrap()
            .contains(&json!("file_type")));
        assert!(item["file_type"].as_str().unwrap().chars().count() <= 256);
    }
    let reply = call(&mut server, 4, "photos_get", json!({"id":"P-ODD-00"})).await;
    assert!(reply_bytes(&reply) <= 65_536);
    let one = text(&reply);
    assert_eq!(
        one["photo"]["file_type"].as_str().unwrap().chars().count(),
        256
    );
    assert_eq!(one["photo_shortened"], true);
}

#[tokio::test]
async fn exactly_ten_thousand_albums_are_not_marked_capped() {
    let (dir, mut server) = bound(None);
    let albums: Vec<Album> = (0..10_000)
        .map(|n| Album {
            id: format!("ALB-{n:05}"),
            name: format!("Album {n}"),
            position: n,
            change_tag: None,
            folder: false,
            parent: None,
        })
        .collect();
    quince_core::Library::open(dir.path())
        .unwrap()
        .replace_albums(&albums)
        .unwrap();
    init(&mut server, false).await;
    let page = ok(&mut server, "photos_albums", json!({"offset":9_999})).await;
    assert_eq!(page["album_index_capped"], false);
    assert_eq!(page["total"], 10_000);
    assert_eq!(ids(&page), ["ALB-09999"]);
}

#[tokio::test]
async fn the_album_index_stops_at_ten_thousand_and_pages_fit() {
    let (dir, mut server) = bound(None);
    let albums: Vec<Album> = (0..10_050)
        .map(|n| Album {
            id: format!("ALB-{n:05}"),
            name: format!("Album {n}"),
            position: n,
            change_tag: None,
            folder: false,
            parent: None,
        })
        .collect();
    quince_core::Library::open(dir.path())
        .unwrap()
        .replace_albums(&albums)
        .unwrap();
    init(&mut server, false).await;
    let reply = call(&mut server, 3, "photos_albums", json!({"limit":100})).await;
    assert!(reply_bytes(&reply) <= 262_144);
    let page = text(&reply);
    assert_eq!(page["album_index_capped"], true);
    assert_eq!(page["total"], 10_000);
    assert_eq!(page["next_offset"], 100);
    let last = ok(
        &mut server,
        "photos_albums",
        json!({"offset":9_950,"limit":100}),
    )
    .await;
    assert_eq!(last["items"].as_array().unwrap().len(), 50);
    assert_eq!(last["items"][49]["id"], "ALB-09999");
    assert!(last["next_offset"].is_null());
    // An album past the listing's cap is still read by its id.
    let beyond = ok(&mut server, "photos_list", json!({"album_id":"ALB-10040"})).await;
    assert_eq!(beyond["album"]["name"], "Album 10040");
}

// --- Approval.

#[tokio::test]
async fn writes_only_and_yolo_read_photos_without_a_prompt() {
    for mode in ["writes_only", "yolo"] {
        let (_dir, mut server) = seeded(Some(mode));
        init(&mut server, true).await;
        for (name, args) in [
            ("photos_albums", json!({})),
            ("photos_list", json!({})),
            ("photos_search", json!({"query":"beach"})),
            ("photos_get", json!({"id":"P-BEACH"})),
        ] {
            let reply = call(&mut server, 3, name, args).await;
            assert!(reply.get("method").is_none(), "{mode} {name}: {reply}");
            assert_eq!(reply["result"]["isError"], false, "{mode} {name}: {reply}");
        }
    }
}

#[tokio::test]
async fn reads_and_writes_binds_each_photos_read_to_one_approval() {
    let (_dir, mut server) = seeded(Some("reads_and_writes"));
    init(&mut server, true).await;
    let form = call(&mut server, 3, "photos_get", json!({"id":"P-BEACH"})).await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    let message = form["params"]["message"].as_str().unwrap();
    assert!(message.contains("photos\\u005fget"), "{message}");
    assert!(!message.contains("Beach day"), "no content before approval");
    assert_eq!(
        text(&call(&mut server, 4, "photos_list", json!({})).await)["error"],
        "approval_pending"
    );
    let accepted = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"accept","content":{}}}))
        .await
        .unwrap();
    assert_eq!(text(&accepted)["photo"]["title"], "Beach day");
    // One approval is one read: the next asks again.
    let form = call(&mut server, 5, "photos_albums", json!({})).await;
    assert_eq!(form["method"], "elicitation/create");
    let denied = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"decline"}}))
        .await
        .unwrap();
    assert_eq!(text(&denied)["error"], "approval_denied");
    // Malformed arguments are refused before anyone is asked.
    assert_eq!(
        text(&call(&mut server, 6, "photos_list", json!({"limit":1000})).await)["error"],
        "invalid_arguments"
    );
    let (_dir, mut plain) = seeded(Some("reads_and_writes"));
    init(&mut plain, false).await;
    assert_eq!(
        error(&mut plain, "photos_search", json!({"query":"beach"})).await,
        "approval_requires_form_client"
    );
}

#[tokio::test]
async fn refresh_prompt_names_photos_only_and_sends_nothing() {
    let (_dir, mut server) = seeded(Some("reads_and_writes"));
    init(&mut server, true).await;
    let form = call(&mut server, 3, "photos_refresh", json!({})).await;
    assert_eq!(form["method"], "elicitation/create", "{form}");
    let message = form["params"]["message"].as_str().unwrap();
    assert!(message.contains("photos\\u005frefresh"), "{message}");
    assert!(
        message.contains("\"services\": \\u005b\n      \"photos\"\n    \\u005d"),
        "{message}"
    );
    for word in ["nothing is sent", "personal", "no pictures"] {
        assert!(message.contains(word), "{word}: {message}");
    }
    for word in ["notes", "reminders", "contacts", "calendar"] {
        assert!(!message.contains(word), "{word}: {message}");
    }
    let reply = server
        .handle(json!({"jsonrpc":"2.0","id":form["id"],"result":{"action":"decline"}}))
        .await
        .unwrap();
    assert_eq!(text(&reply)["error"], "approval_denied");
}

#[tokio::test]
async fn refresh_validates_before_any_keyring_or_network_use() {
    let dir = private_dir();
    let mut server = Server::open(dir.path()).unwrap();
    init(&mut server, false).await;
    assert_eq!(
        error(&mut server, "photos_refresh", json!({})).await,
        "login_required"
    );
    assert_eq!(
        error(&mut server, "photos_refresh", json!({"all":true})).await,
        "invalid_arguments"
    );
}

// --- Refresh against a synthetic Apple.

fn b64(text: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(text)
}

fn keywords(words: &[&str]) -> String {
    let items: String = words
        .iter()
        .map(|w| format!("<string>{w}</string>"))
        .collect();
    b64(&format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><array>{items}</array></plist>"#
    ))
}

fn place() -> String {
    b64(
        r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>lat</key><real>52.123456</real><key>lon</key><real>13.654321</real></dict></plist>"#,
    )
}

fn res(name: &str) -> Value {
    json!({"type":"ASSETID","value":{"downloadURL":format!("https://synthetic.invalid/{name}/${{f}}"),"size":10,"fileChecksum":"c"}})
}

fn master(id: &str, file: &str, uti: &str) -> Value {
    json!({"recordName":id,"recordType":"CPLMaster","recordChangeTag":"m1","fields":{
        "filenameEnc":{"type":"ENCRYPTED_BYTES","value":b64(file)},
        "itemType":{"type":"STRING","value":uti},
        "resOriginalWidth":{"type":"INT64","value":4032},
        "resOriginalHeight":{"type":"INT64","value":3024},
        "resOriginalFileSize":{"type":"INT64","value":2_000_000},
        "resOriginalRes":res("orig"),
        "resJPEGThumbRes":res("thumb"),
        "resJPEGMedRes":res("med"),
    }})
}

fn live_master(id: &str) -> Value {
    let mut m = master(id, "LIVE.HEIC", "public.heic");
    m["fields"]["resOriginalVidComplRes"] = res("live");
    m["fields"]["resOriginalVidComplFileSize"] = json!({"type":"INT64","value":1_500_000});
    m
}

fn video_master(id: &str) -> Value {
    let mut m = master(id, "CLIP.MOV", "com.apple.quicktime-movie");
    m["fields"]["resVidMedRes"] = res("vid");
    m
}

fn asset(id: &str, master: &str, taken_ms: i64) -> Value {
    json!({"recordName":id,"recordType":"CPLAsset","recordChangeTag":"a1","fields":{
        "masterRef":{"type":"REFERENCE","value":{"recordName":master,"action":"DELETE_SELF"}},
        "assetDate":{"type":"TIMESTAMP","value":taken_ms},
        "addedDate":{"type":"TIMESTAMP","value":taken_ms + 1000},
        "locationEnc":{"type":"ENCRYPTED_BYTES","value":place()},
        "resJPEGFullRes":res("edit"),
    }})
}

fn album(id: &str, name: &str) -> Value {
    json!({"recordName":id,"recordType":"CPLAlbum","recordChangeTag":"b1","fields":{
        "albumNameEnc":{"type":"ENCRYPTED_BYTES","value":b64(name)},
        "albumType":{"type":"INT64","value":0},
        "position":{"type":"INT64","value":1},
    }})
}

fn relation(album: &str, asset: &str, position: i64) -> Value {
    json!({"recordName":format!("{asset}-IN-{album}"),"recordType":"CPLContainerRelation","fields":{
        "containerId":{"type":"STRING","value":album},
        "itemId":{"type":"STRING","value":asset},
        "position":{"type":"INT64","value":position},
    }})
}

/// A whole small library, Apple-shaped: an image with a caption and keywords, a Live Photo,
/// a video, a hidden and a deleted photo, an album, a face, and an asset whose master is
/// missing.
fn library_records() -> Vec<Value> {
    let mut beach = asset("A-BEACH", "M-BEACH", 1_700_000_300_000);
    beach["fields"]["captionEnc"] = json!({"type":"ENCRYPTED_BYTES","value":b64("Beach day")});
    beach["fields"]["extendedDescEnc"] = json!({"type":"ENCRYPTED_BYTES","value":b64("Sand")});
    beach["fields"]["keywordsEnc"] =
        json!({"type":"ENCRYPTED_BYTES","value":keywords(&["summer","sea"])});
    beach["fields"]["isFavorite"] = json!({"type":"INT64","value":1});
    let mut hidden = asset("A-HIDDEN", "M-HIDDEN", 1_700_000_400_000);
    hidden["fields"]["isHidden"] = json!({"type":"INT64","value":1});
    let mut deleted = asset("A-DELETED", "M-DELETED", 1_700_000_500_000);
    deleted["fields"]["isDeleted"] = json!({"type":"INT64","value":1});
    let mut clip = asset("A-VIDEO", "M-VIDEO", 1_700_000_100_000);
    clip["fields"]["duration"] = json!({"type":"INT64","value":12917});
    vec![
        master("M-BEACH", "BEACH.HEIC", "public.heic"),
        beach,
        live_master("M-LIVE"),
        asset("A-LIVE", "M-LIVE", 1_700_000_200_000),
        video_master("M-VIDEO"),
        clip,
        master("M-HIDDEN", "H.HEIC", "public.heic"),
        hidden,
        master("M-DELETED", "D.HEIC", "public.heic"),
        deleted,
        album("ALB-TRIP", "Trip"),
        relation("ALB-TRIP", "A-BEACH", 0),
        json!({"recordName":"FACE-1","recordType":"CPLFace","fields":{"personName":{"value":"SYNTHETIC-PERSON"}}}),
        json!({"recordName":"PERSON-1","recordType":"CPLPerson","fields":{"fullName":{"value":b64("SYNTHETIC-PERSON")}}}),
        asset("A-ORPHAN", "M-MISSING", 1_700_000_600_000),
    ]
}

/// Matches a `changes/zone` request by the change token it carries.
struct Token(Option<&'static str>);
impl wiremock::Match for Token {
    fn matches(&self, request: &Request) -> bool {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        body["zones"][0]["syncToken"].as_str() == self.0
    }
}

fn changes(records: &[Value], token: &str, more: bool) -> Value {
    json!({"zones":[{"zoneID":{"zoneName":"PrimarySync"},"records":records,"syncToken":token,"moreComing":more}]})
}

async fn apple() -> MockServer {
    let remote = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},
            "webservices":{"ckdatabasews":{"url":remote.uri()}}
        })))
        .mount(&remote)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{DB}/zones/list")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"zones":[
            {"zoneID":{"zoneName":"PrimarySync"}},
            {"zoneID":{"zoneName":"SharedSync-SYNTHETIC"}}
        ]})))
        .mount(&remote)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{DB}/records/query")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"records":[
            {"recordName":"CheckIndexingState","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}
        ]})))
        .mount(&remote)
        .await;
    remote
}

async fn page(remote: &MockServer, from: Option<&'static str>, body: Value) {
    Mock::given(method("POST"))
        .and(path(format!("{DB}/changes/zone")))
        .and(Token(from))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(remote)
        .await;
}

fn saved_client(dir: &std::path::Path, uri: &str) -> Client {
    saved_client_with(dir, uri, Session::default())
}

fn saved_client_with(dir: &std::path::Path, uri: &str, base: Session) -> Client {
    let store = Store::file(dir).unwrap().without_passwords();
    store
        .save(&Session {
            dsid: Some(DSID.into()),
            session_token: Some("SYNTHETIC-TOKEN".into()),
            webservices: Some(json!({"ckdatabasews":{"url":uri}})),
            ..base
        })
        .unwrap();
    Client::with(dir, Endpoints::at(uri), store).unwrap()
}

fn private_files(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    for entry in std::fs::read_dir(dir).unwrap() {
        std::fs::set_permissions(
            entry.unwrap().path(),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
}

async fn refresh(dir: &std::path::Path, uri: &str) -> Result<Value, &'static str> {
    let result = skykeep_mcp::photos::refresh_with_client(dir, saved_client(dir, uri)).await;
    private_files(dir);
    result
}

async fn refresh_limited(
    dir: &std::path::Path,
    uri: &str,
    limits: skykeep_mcp::photos::RefreshLimits,
) -> Result<Value, &'static str> {
    let result =
        skykeep_mcp::photos::refresh_with_client_limited(dir, saved_client(dir, uri), limits).await;
    private_files(dir);
    result
}

/// Unrelated state and queued changes of every service, Photos' own included.
fn with_unrelated_state(dir: &std::path::Path) {
    let mut library = quince_core::Library::open(dir).unwrap();
    library.mark_synced("notes", Some("NOTES-CURSOR")).unwrap();
    library
        .mark_synced("reminders", Some("REMINDERS-CURSOR"))
        .unwrap();
    library
        .mark_synced("contacts", Some("CONTACTS-CURSOR"))
        .unwrap();
    library
        .mark_synced("calendar", Some("CALENDAR-CURSOR"))
        .unwrap();
    for (service, record) in [
        ("notes", "N-1"),
        ("reminders", "R-1"),
        ("photos", "A-BEACH"),
    ] {
        library
            .outbox_append(service, record, "favorite", "{\"queued\":true}")
            .unwrap();
    }
}

fn unrelated(dir: &std::path::Path) -> Value {
    let library = quince_core::Library::open(dir).unwrap();
    let pending: Vec<Value> = library
        .outbox_all_pending()
        .unwrap()
        .into_iter()
        .map(|p| json!([p.id, p.service, p.record, p.kind, p.payload, p.tries]))
        .collect();
    let states: Vec<Value> = ["notes", "reminders", "contacts", "calendar"]
        .iter()
        .map(|s| {
            json!([
                library.sync_token(s).unwrap(),
                library.last_synced(s).unwrap()
            ])
        })
        .collect();
    json!({"pending":pending,"refused":library.outbox_refused_count().unwrap(),"states":states,
        "policy":library.state("mcp.policy").unwrap()})
}

/// The Photos cache and where its refresh got to.
fn cached(dir: &std::path::Path) -> Value {
    let library = quince_core::Library::open(dir).unwrap();
    let mut photos: Vec<Value> = library
        .photo_ids()
        .unwrap()
        .into_iter()
        .map(|id| serde_json::to_value(library.photo(&id).unwrap()).unwrap())
        .collect();
    photos.sort_by_key(|p| p["id"].as_str().unwrap().to_string());
    json!({
        "photos": photos,
        "albums": library.albums_counted().unwrap().into_iter().map(|(a, n, _)| json!([a.id, a.name, n])).collect::<Vec<_>>(),
        "token": library.sync_token("photos").unwrap(),
        "walk": library.photo_walk_cursor(None).unwrap(),
        "held": library.photo_walk_held(None).unwrap(),
    })
}

fn photos_failed(dir: &std::path::Path) -> bool {
    quince_core::Library::open(dir)
        .unwrap()
        .sync_health()
        .unwrap()
        .iter()
        .any(|h| h.service == "photos" && h.last_error.is_some())
}

/// Only reads of the personal library: POSTs to the Photos container's private database.
fn only_photo_reads(requests: &[Request]) {
    for r in requests {
        let p = r.url.path();
        if p == "/setup/ws/1/validate" {
            continue;
        }
        assert_eq!(r.method.as_str(), "POST", "unexpected {} {p}", r.method);
        assert!(
            [
                format!("{DB}/zones/list"),
                format!("{DB}/records/query"),
                format!("{DB}/changes/zone")
            ]
            .contains(&p.to_string()),
            "unexpected {p}"
        );
        if p.ends_with("changes/zone") {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            assert_eq!(body["zones"][0]["zoneID"]["zoneName"], "PrimarySync");
            assert_eq!(body["zones"][0]["maxChanges"], 500);
            assert!(body["zones"][0]["zoneID"].get("ownerRecordName").is_none());
        }
    }
}

#[tokio::test]
async fn refresh_reads_the_personal_library_without_places_links_or_sending() {
    for policy in [None, Some("writes_only"), Some("yolo")] {
        let remote = apple().await;
        page(&remote, None, changes(&library_records(), "T-1", false)).await;
        let (dir, mut server) = bound(policy);
        with_unrelated_state(dir.path());
        let before = unrelated(dir.path());
        let done = refresh(dir.path(), &remote.uri()).await.unwrap();
        assert_eq!(done["outcome"], "ok", "{done}");
        assert_eq!(done["read"], "full");
        assert_eq!(done["complete"], true);
        assert_eq!(done["more"], false);
        assert_eq!(done["pages_read"], 1);
        assert_eq!(done["cached_photos"], 3);
        assert_eq!(done["cached_albums"], 1);
        assert_eq!(done["queued_photo_changes"], 1);
        assert_eq!(done["queued_changes_sent"], 0);
        assert_eq!(done["remote_verified"], false);
        nothing_private(&done);
        assert_eq!(unrelated(dir.path()), before, "{policy:?}");
        only_photo_reads(&remote.received_requests().await.unwrap());
        // Nothing of a place or a download link reached the cache.
        let library = quince_core::Library::open(dir.path()).unwrap();
        for id in library.photo_ids().unwrap() {
            let p = library.photo(&id).unwrap().unwrap();
            assert!(p.latitude.is_none() && p.longitude.is_none(), "{id}");
        }
        assert!(library.photo("A-ORPHAN").unwrap().is_none());
        assert_eq!(
            library.sync_token("photos").unwrap().as_deref(),
            Some("T-1")
        );
        drop(library);
        init(&mut server, false).await;
        let page = ok(&mut server, "photos_list", json!({})).await;
        assert_eq!(ids(&page), ["A-BEACH", "A-LIVE", "A-VIDEO"]);
        let items = page["items"].as_array().unwrap();
        assert_eq!(items[0]["title"], "Beach day");
        assert_eq!(items[0]["keywords"], json!(["summer", "sea"]));
        assert_eq!(items[1]["kind"], "live_photo");
        assert_eq!(items[2]["kind"], "video");
        assert_eq!(items[2]["duration_seconds"], 12.917);
        nothing_private(&page);
        let trip = ok(&mut server, "photos_list", json!({"album_id":"ALB-TRIP"})).await;
        assert_eq!(ids(&trip), ["A-BEACH"]);
        assert_eq!(page["cache"]["refreshed"], true);
        assert_eq!(page["cache"]["last_refresh_failed"], false);
    }
}

#[tokio::test]
async fn a_large_library_stops_at_ten_pages_and_the_next_refresh_carries_on() {
    let remote = apple().await;
    let tokens: [&'static str; 12] = [
        "T-01", "T-02", "T-03", "T-04", "T-05", "T-06", "T-07", "T-08", "T-09", "T-10", "T-11",
        "T-12",
    ];
    // Page n carries photo n. The master of the tenth page's photo comes on the eleventh.
    for n in 0..12 {
        let from = if n == 0 { None } else { Some(tokens[n - 1]) };
        let id = format!("A-{n:02}");
        let records = if n == 9 {
            vec![asset(&id, "M-09", 1_700_000_000_000 + n as i64 * 1000)]
        } else if n == 10 {
            vec![
                master("M-09", "NINE.HEIC", "public.heic"),
                master(&format!("M-{n:02}"), "X.HEIC", "public.heic"),
                asset(
                    &id,
                    &format!("M-{n:02}"),
                    1_700_000_000_000 + n as i64 * 1000,
                ),
            ]
        } else {
            vec![
                master(&format!("M-{n:02}"), "X.HEIC", "public.heic"),
                asset(
                    &id,
                    &format!("M-{n:02}"),
                    1_700_000_000_000 + n as i64 * 1000,
                ),
            ]
        };
        page(&remote, from, changes(&records, tokens[n], n < 11)).await;
    }
    let (dir, _server) = bound(Some("yolo"));
    let first = refresh(dir.path(), &remote.uri()).await.unwrap();
    assert_eq!(first["complete"], false, "{first}");
    assert_eq!(first["more"], true);
    assert_eq!(first["pages_read"], 10);
    assert!(first["continue"]
        .as_str()
        .unwrap()
        .contains("photos_refresh"));
    let state = cached(dir.path());
    assert_eq!(state["token"], "T-10");
    assert_eq!(state["walk"], "T-10");
    assert_eq!(state["photos"].as_array().unwrap().len(), 9);
    // What waits for its other half is held without its download link or place.
    let held = state["held"].to_string();
    assert!(held.contains("A-09"), "{held}");
    assert!(!held.contains("downloadURL") && !held.contains("locationEnc"));
    assert!(!held.contains("synthetic.invalid"));
    let second = refresh(dir.path(), &remote.uri()).await.unwrap();
    assert_eq!(second["complete"], true, "{second}");
    assert_eq!(second["read"], "changes");
    assert_eq!(second["pages_read"], 2);
    let state = cached(dir.path());
    assert_eq!(state["token"], "T-12");
    assert_eq!(state["photos"].as_array().unwrap().len(), 12);
    let asked: Vec<Option<String>> = remote
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("changes/zone"))
        .map(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            body["zones"][0]["syncToken"].as_str().map(str::to_string)
        })
        .collect();
    assert_eq!(asked.len(), 12);
    assert_eq!(asked[10].as_deref(), Some("T-10"));
}

#[tokio::test]
async fn a_failure_part_way_keeps_the_cache_and_cursor_and_apple_text_out() {
    let remote = apple().await;
    page(&remote, None, changes(&library_records(), "T-1", false)).await;
    let (dir, mut server) = bound(Some("writes_only"));
    with_unrelated_state(dir.path());
    refresh(dir.path(), &remote.uri()).await.unwrap();
    let kept = cached(dir.path());
    let before = unrelated(dir.path());
    // From T-1: two good pages that delete the beach photo, then Apple fails.
    let gone = json!({"recordName":"A-BEACH","deleted":true});
    page(&remote, Some("T-1"), changes(&[gone], "T-2", true)).await;
    page(&remote, Some("T-2"), changes(&[], "T-3", true)).await;
    Mock::given(method("POST"))
        .and(path(format!("{DB}/changes/zone")))
        .and(Token(Some("T-3")))
        .respond_with(ResponseTemplate::new(503).set_body_string("SYNTHETIC-UPSTREAM-BODY"))
        .mount(&remote)
        .await;
    assert_eq!(
        refresh(dir.path(), &remote.uri()).await.unwrap_err(),
        "photos_refresh_failed_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
    assert_eq!(unrelated(dir.path()), before);
    assert!(photos_failed(dir.path()));
    let health = quince_core::Library::open(dir.path())
        .unwrap()
        .sync_health()
        .unwrap();
    assert!(!format!("{health:?}").contains("SYNTHETIC-UPSTREAM-BODY"));
    init(&mut server, false).await;
    let page = ok(&mut server, "photos_list", json!({})).await;
    assert!(ids(&page).contains(&"A-BEACH".to_string()));
    assert_eq!(page["cache"]["last_refresh_failed"], true);
    assert!(!page.to_string().contains("SYNTHETIC-UPSTREAM-BODY"));
}

#[tokio::test]
async fn a_continuation_that_does_not_move_is_refused_and_nothing_stored() {
    let remote = apple().await;
    page(&remote, None, changes(&library_records(), "T-SAME", true)).await;
    page(&remote, Some("T-SAME"), changes(&[], "T-SAME", true)).await;
    let (dir, _server) = bound(Some("yolo"));
    let kept = cached(dir.path());
    assert_eq!(
        refresh(dir.path(), &remote.uri()).await.unwrap_err(),
        "photos_bad_continuation_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
    assert!(photos_failed(dir.path()));
}

#[tokio::test]
async fn a_refused_change_token_reads_again_from_the_start() {
    let remote = apple().await;
    page(&remote, None, changes(&library_records(), "T-1", false)).await;
    let (dir, _server) = bound(Some("yolo"));
    refresh(dir.path(), &remote.uri()).await.unwrap();
    let again = apple().await;
    Mock::given(method("POST"))
        .and(path(format!("{DB}/changes/zone")))
        .and(Token(Some("T-1")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"zones":[
            {"zoneID":{"zoneName":"PrimarySync"},"serverErrorCode":"CHANGE_TOKEN_EXPIRED","reason":"expired"}
        ]})))
        .mount(&again)
        .await;
    let fewer: Vec<Value> = library_records()
        .into_iter()
        .filter(|r| !r["recordName"].as_str().unwrap().contains("LIVE"))
        .collect();
    page(&again, None, changes(&fewer, "T-NEW", false)).await;
    let done = refresh(dir.path(), &again.uri()).await.unwrap();
    assert_eq!(done["read"], "full", "{done}");
    let state = cached(dir.path());
    assert_eq!(state["token"], "T-NEW");
    // A whole read lets go of what it never saw.
    let library = quince_core::Library::open(dir.path()).unwrap();
    assert!(library.photo("A-LIVE").unwrap().is_none());
    assert!(library.photo("A-BEACH").unwrap().is_some());
}

#[tokio::test]
async fn oversized_answers_are_refused_while_counted_and_the_cache_kept() {
    let small = skykeep_mcp::photos::RefreshLimits {
        max_response_bytes: 4096,
        max_small_bytes: 1024,
        max_total_bytes: 64 * 1024,
        ..Default::default()
    };
    // One page over its cap.
    let remote = apple().await;
    let mut big = asset("A-BIG", "M-BIG", 1_700_000_000_000);
    big["fields"]["captionEnc"] = json!({"value":b64(&"x".repeat(8000))});
    page(
        &remote,
        None,
        changes(&[master("M-BIG", "B", "public.heic"), big], "T-1", false),
    )
    .await;
    let (dir, _server) = bound(Some("yolo"));
    let kept = cached(dir.path());
    assert_eq!(
        refresh_limited(dir.path(), &remote.uri(), small)
            .await
            .unwrap_err(),
        "photos_response_too_large_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
    assert!(photos_failed(dir.path()));
    // A zone list over its own, smaller cap.
    let remote = MockServer::start().await;
    let uri = remote.uri();
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},"webservices":{"ckdatabasews":{"url":uri}}
        })))
        .mount(&remote)
        .await;
    let zones: Vec<Value> = (0..100)
        .map(|n| json!({"zoneID":{"zoneName":format!("Zone-{n:04}")}}))
        .collect();
    Mock::given(method("POST"))
        .and(path(format!("{DB}/zones/list")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"zones":zones})))
        .mount(&remote)
        .await;
    let (dir, _server) = bound(Some("yolo"));
    assert_eq!(
        refresh_limited(dir.path(), &remote.uri(), small)
            .await
            .unwrap_err(),
        "photos_response_too_large_cache_kept"
    );
    assert!(remote
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| !r.url.path().ends_with("changes/zone")));
    // Pages each under their cap, but together over the call's.
    let remote = apple().await;
    let tokens: [&'static str; 6] = ["U-1", "U-2", "U-3", "U-4", "U-5", "U-6"];
    for n in 0..6 {
        let from = if n == 0 { None } else { Some(tokens[n - 1]) };
        let mut a = asset(&format!("A-{n}"), &format!("M-{n}"), 1_700_000_000_000);
        a["fields"]["captionEnc"] = json!({"value":b64(&"y".repeat(2400))});
        page(
            &remote,
            from,
            changes(
                &[master(&format!("M-{n}"), "F", "public.heic"), a],
                tokens[n],
                n < 5,
            ),
        )
        .await;
    }
    let (dir, _server) = bound(Some("yolo"));
    let kept = cached(dir.path());
    let tight = skykeep_mcp::photos::RefreshLimits {
        max_total_bytes: 12 * 1024,
        ..small
    };
    assert_eq!(
        refresh_limited(dir.path(), &remote.uri(), tight)
            .await
            .unwrap_err(),
        "photos_response_too_large_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
}

#[tokio::test]
async fn advanced_data_protection_pauses_the_refresh_and_stores_nothing() {
    // Apple's own refusal, inside the zone's result.
    let remote = apple().await;
    Mock::given(method("POST"))
        .and(path(format!("{DB}/changes/zone")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"zones":[{
            "zoneID":{"zoneName":"PrimarySync"},"serverErrorCode":"ACCESS_DENIED",
            "reason":"private db access disabled for this account"}]})))
        .mount(&remote)
        .await;
    let (dir, _server) = seeded(Some("yolo"));
    let kept = cached(dir.path());
    assert_eq!(
        refresh(dir.path(), &remote.uri()).await.unwrap_err(),
        "photos_paused_approve_on_apple_device"
    );
    assert_eq!(cached(dir.path()), kept);
    assert!(!photos_failed(dir.path()));
    // And as a refused request.
    let remote = apple().await;
    Mock::given(method("POST"))
        .and(path(format!("{DB}/changes/zone")))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "serverErrorCode":"ACCESS_DENIED","reason":"private db access disabled for this account"})))
        .mount(&remote)
        .await;
    assert_eq!(
        refresh(dir.path(), &remote.uri()).await.unwrap_err(),
        "photos_paused_approve_on_apple_device"
    );
    assert_eq!(cached(dir.path()), kept);
    // A session known to have Advanced Data Protection and no Photos key asks Apple nothing.
    let remote = apple().await;
    let client = saved_client_with(
        dir.path(),
        &remote.uri(),
        Session {
            data_protection_seen: Some(true),
            ..Default::default()
        },
    );
    assert_eq!(
        skykeep_mcp::photos::refresh_with_client(dir.path(), client)
            .await
            .unwrap_err(),
        "photos_paused_approve_on_apple_device"
    );
    private_files(dir.path());
    assert!(remote
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| !r.url.path().starts_with("/database/")));
    assert_eq!(cached(dir.path()), kept);
}

#[tokio::test]
async fn a_library_apple_is_still_preparing_is_not_read() {
    let remote = MockServer::start().await;
    let uri = remote.uri();
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},"webservices":{"ckdatabasews":{"url":uri}}
        })))
        .mount(&remote)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{DB}/zones/list")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"zones":[{"zoneID":{"zoneName":"PrimarySync"}}]})),
        )
        .mount(&remote)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{DB}/records/query")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"records":[
            {"recordType":"CheckIndexingState","fields":{"state":{"value":"RUNNING"},"progress":{"value":40}}}
        ]})))
        .mount(&remote)
        .await;
    let (dir, _server) = seeded(Some("yolo"));
    let kept = cached(dir.path());
    assert_eq!(
        refresh(dir.path(), &remote.uri()).await.unwrap_err(),
        "photos_library_preparing_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
    assert!(remote
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| !r.url.path().ends_with("changes/zone")));
}

#[tokio::test]
async fn refresh_fails_closed_on_session_account_or_service_problems() {
    // Apple ends the session.
    let remote = apple().await;
    Mock::given(method("POST"))
        .and(path(format!("{DB}/changes/zone")))
        .respond_with(ResponseTemplate::new(450))
        .mount(&remote)
        .await;
    let (dir, _server) = seeded(Some("writes_only"));
    let kept = cached(dir.path());
    assert_eq!(
        refresh(dir.path(), &remote.uri()).await.unwrap_err(),
        "login_required"
    );
    assert_eq!(cached(dir.path()), kept);
    // A session for another account reads nothing.
    let remote = apple().await;
    let dir = private_dir();
    let mut library = quince_core::Library::open(dir.path()).unwrap();
    library
        .set_state("account", &quince_sync::queue::account_tag("9999999999"))
        .unwrap();
    seed(&mut library);
    let kept = cached(dir.path());
    assert_eq!(
        refresh(dir.path(), &remote.uri()).await.unwrap_err(),
        "account_mismatch_use_new_state_directory"
    );
    assert!(remote
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| !r.url.path().starts_with("/database/")));
    assert_eq!(cached(dir.path()), kept);
    // No CloudKit service in the session.
    let remote = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/setup/ws/1/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "dsInfo":{"dsid":DSID},"webservices":{"contacts":{"url":remote.uri()}}
        })))
        .mount(&remote)
        .await;
    let (dir, _server) = bound(None);
    let store = Store::file(dir.path()).unwrap().without_passwords();
    store
        .save(&Session {
            dsid: Some(DSID.into()),
            session_token: Some("SYNTHETIC-TOKEN".into()),
            webservices: Some(json!({"contacts":{"url":remote.uri()}})),
            ..Default::default()
        })
        .unwrap();
    let client = Client::with(dir.path(), Endpoints::at(&remote.uri()), store).unwrap();
    assert_eq!(
        skykeep_mcp::photos::refresh_with_client(dir.path(), client)
            .await
            .unwrap_err(),
        "photos_service_unavailable"
    );
    // A library zone Apple does not have is not read as an empty library.
    let remote = apple().await;
    Mock::given(method("POST"))
        .and(path(format!("{DB}/changes/zone")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"zones":[{
            "zoneID":{"zoneName":"PrimarySync"},"serverErrorCode":"ZONE_NOT_FOUND"}]})))
        .mount(&remote)
        .await;
    let (dir, _server) = seeded(Some("yolo"));
    let kept = cached(dir.path());
    assert_eq!(
        refresh(dir.path(), &remote.uri()).await.unwrap_err(),
        "photos_library_unavailable_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
    // Unbound directory: nothing is resumed or read.
    let remote = apple().await;
    let dir = private_dir();
    assert_eq!(
        refresh(dir.path(), &remote.uri()).await.unwrap_err(),
        "login_required"
    );
    assert!(remote.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn reads_and_writes_refresh_without_approval_touches_nothing() {
    let remote = apple().await;
    let (dir, _server) = bound(Some("reads_and_writes"));
    with_unrelated_state(dir.path());
    let before = unrelated(dir.path());
    let kept = cached(dir.path());
    assert_eq!(
        refresh(dir.path(), &remote.uri()).await.unwrap_err(),
        "read_approval_required"
    );
    assert!(remote.received_requests().await.unwrap().is_empty());
    assert_eq!(unrelated(dir.path()), before);
    assert_eq!(cached(dir.path()), kept);
}

#[tokio::test]
async fn a_slow_answer_times_out_stores_nothing_and_is_recorded() {
    let remote = apple().await;
    Mock::given(method("POST"))
        .and(path(format!("{DB}/changes/zone")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(changes(&library_records(), "T-1", false))
                .set_delay(std::time::Duration::from_secs(5)),
        )
        .mount(&remote)
        .await;
    let (dir, _server) = seeded(Some("yolo"));
    let kept = cached(dir.path());
    let limits = skykeep_mcp::photos::RefreshLimits {
        timeout: std::time::Duration::from_millis(300),
        ..Default::default()
    };
    assert_eq!(
        refresh_limited(dir.path(), &remote.uri(), limits)
            .await
            .unwrap_err(),
        "photos_refresh_timeout_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
    assert!(photos_failed(dir.path()));
}

/// A clock that reads as past any deadline from its second reading on: the read finishes in
/// time, and the deadline has passed when the store would begin.
fn late_clock() -> std::time::Instant {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static READS: AtomicUsize = AtomicUsize::new(0);
    let now = std::time::Instant::now();
    if READS.fetch_add(1, Ordering::SeqCst) == 0 {
        now
    } else {
        now + std::time::Duration::from_secs(3600)
    }
}

#[tokio::test]
async fn a_deadline_passed_before_the_store_stores_nothing_and_is_recorded() {
    let remote = apple().await;
    page(&remote, None, changes(&library_records(), "T-1", false)).await;
    let (dir, _server) = seeded(Some("yolo"));
    let kept = cached(dir.path());
    let limits = skykeep_mcp::photos::RefreshLimits {
        clock: late_clock,
        ..Default::default()
    };
    assert_eq!(
        refresh_limited(dir.path(), &remote.uri(), limits)
            .await
            .unwrap_err(),
        "photos_refresh_timeout_cache_kept"
    );
    assert_eq!(cached(dir.path()), kept);
    assert!(photos_failed(dir.path()));
}

#[tokio::test]
async fn an_account_change_during_refresh_stores_nothing() {
    let remote = apple().await;
    Mock::given(method("POST"))
        .and(path(format!("{DB}/changes/zone")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(changes(&library_records(), "T-1", false))
                .set_delay(std::time::Duration::from_millis(400)),
        )
        .mount(&remote)
        .await;
    let (dir, _server) = bound(Some("yolo"));
    let path_buf = dir.path().to_owned();
    let uri = remote.uri();
    let running = tokio::spawn(async move { refresh(&path_buf, &uri).await });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    quince_core::Library::open(dir.path())
        .unwrap()
        .set_state("account", &quince_sync::queue::account_tag("9999999999"))
        .unwrap();
    assert_eq!(
        running.await.unwrap().unwrap_err(),
        "account_changed_during_refresh_cache_kept"
    );
    let library = quince_core::Library::open(dir.path()).unwrap();
    assert!(library.photo_ids().unwrap().is_empty());
    assert!(library.sync_token("photos").unwrap().is_none());
    assert!(!photos_failed(dir.path()));
}
