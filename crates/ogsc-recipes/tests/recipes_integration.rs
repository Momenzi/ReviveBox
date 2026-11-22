//! Integration tests verifying shipped game recipes and generated JSON Schema parity.

use std::path::PathBuf;

use ogsc_recipes::{GameRecipe, RecipeDatabase, ValidationCode};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

#[test]
fn all_shipped_recipes_load_and_roundtrip_cleanly() {
    let games_dir = repo_root().join("recipes").join("games");
    let loaded = RecipeDatabase::load_from_dir(&games_dir, false)
        .expect("shipped recipes should pass non-strict validation");

    assert_eq!(loaded.db.len(), 4);
    let barbie = loaded
        .db
        .get_by_id("barbie-diaries-high-school-mystery-2006")
        .expect("Barbie Diaries recipe should be present");
    assert_eq!(barbie.fixes.len(), 4);
    assert_eq!(barbie.fixes_for_release("retail-cd-en-2006").len(), 4);
    assert_eq!(loaded.db.find_by_executable_name("game.exe").len(), 1);
    assert_eq!(
        loaded
            .db
            .find_by_sha256("9da8b4992a98b4fdbefed8a88c5e14c48f43268b2f429641ae606d5d95c0eb26")
            .expect("Barbie SHA-256 must be indexed")
            .game_id,
        "barbie-diaries-high-school-mystery-2006"
    );
    assert!(barbie.metadata.cover_art_url.is_some(), "Barbie recipe should define cover_art_url");
    let (_, barbie_warnings) = RecipeDatabase::load_recipe_file(
        &games_dir.join("barbie_diaries_high_school_mystery_2006.json"),
        true,
    )
    .expect("verified Barbie recipe must pass strict validation");
    assert_eq!(barbie_warnings.len(), 0);
    let nfs = loaded.db.get_by_id("nfs-most-wanted-2005").expect("NFS MW recipe should be present");
    assert!(nfs.metadata.cover_art_url.is_some(), "NFS MW recipe should define cover_art_url");
    assert_eq!(
        loaded
            .db
            .find_by_sha256("c60f1d0f65bb81d22579ff5e14108bb66a0bddb9587fb16c339cdd15e6e34d35")
            .expect("NFS EU/PAL SHA-256 must be indexed")
            .release_id,
        "retail-eu-pal-2005"
    );
    assert_eq!(nfs.fixes.len(), 5);
    assert_eq!(nfs.fixes_for_release("retail-cd-v1.3").len(), 5);
    assert_eq!(
        nfs.steps_for_selected_fixes(
            "retail-cd-v1.3",
            &["widescreen_fix".to_owned(), "xinput_plus".to_owned()],
        )
        .len(),
        4
    );

    verify_darkspore_recipe(&games_dir, &loaded.db);

    let sc = loaded
        .db
        .get_by_id("splinter-cell-pandora-tomorrow-2004")
        .expect("SC PT recipe should be present");
    assert_eq!(sc.fixes.len(), 3);
    assert_eq!(sc.fixes_for_release("retail-dvd-v1.31").len(), 3);

    // Case-insensitive executable lookup
    let nfs_matches = loaded.db.find_by_executable_name("SPEED.EXE");
    assert_eq!(nfs_matches.len(), 3);

    let sc_matches = loaded.db.find_by_executable_name("splintercell2.exe");
    assert_eq!(sc_matches.len(), 1);
    assert_eq!(sc_matches[0].game_id, "splinter-cell-pandora-tomorrow-2004");

    // Full SHA-256 lookup
    let hash_match = loaded
        .db
        .find_by_sha256("0000000000000000000000000000000000000000000000000000000000000001")
        .expect("template SHA-256 should be indexed");
    assert_eq!(hash_match.release_id, "retail-cd-v1.3");

    // Strict mode must fail while template hashes remain unverified
    let strict_err = RecipeDatabase::load_from_dir(&games_dir, true)
        .expect_err("strict mode should flag unverified_template hashes");
    assert!(strict_err.to_string().contains("UnverifiedTemplateHash"));

    // Round-trip serialization equality for every shipped recipe
    for recipe in loaded.db.all_recipes() {
        assert!(
            recipe.metadata.required_game_version.is_some(),
            "shipped recipe `{}` should specify required_game_version",
            recipe.game_id
        );
        let serialized = serde_json::to_string_pretty(recipe).expect("serialize recipe");
        let parsed: GameRecipe = serde_json::from_str(&serialized).expect("re-parse recipe");
        assert_eq!(recipe, &parsed);
    }

    // Confirm shipped recipes have zero non-template warnings
    for record in &loaded.warnings {
        assert!(
            record.warnings.iter().all(|w| w.code == ValidationCode::UnverifiedTemplateHash),
            "unexpected warning in `{}`: {:?}",
            record.game_id,
            record.warnings
        );
    }
}

fn verify_darkspore_recipe(games_dir: &std::path::Path, db: &RecipeDatabase) {
    let darkspore = db.get_by_id("darkspore-2011").expect("Darkspore recipe should be present");
    assert!(
        darkspore.metadata.cover_art_url.is_some(),
        "Darkspore recipe should define cover_art_url"
    );
    assert_eq!(
        db.find_by_sha256("3c7248a4c6614450290a66c22ab7fcda86052b29a7b555e834d7c23f9b73ee5b")
            .expect("Darkspore Steam v5.3.0.103 SHA-256 must be indexed")
            .release_id,
        "steam-5.3.0.103"
    );
    assert_eq!(
        db.find_by_sha256("3b499380f0cce50922003ad59b456d9ef7c47c3d08ecc9cb683868d94a4dc2d8")
            .expect("Darkspore DVD v5.3.0.15 SHA-256 must be indexed")
            .release_id,
        "retail-dvd-5.3.0.15"
    );
    assert_eq!(
        darkspore
            .executable_for_launch("retail-dvd-5.3.0.15", &["resurrection_capsule".to_owned()]),
        "DarksporeBin/RecapLauncher.exe"
    );
    assert_eq!(
        darkspore.executable_for_launch("retail-dvd-5.3.0.15", &[]),
        "DarksporeBin/Darkspore.exe"
    );
    let (_, darkspore_warnings) =
        RecipeDatabase::load_recipe_file(&games_dir.join("darkspore_2011.json"), true)
            .expect("verified Darkspore recipe must pass strict validation");
    assert_eq!(darkspore_warnings.len(), 0);
    assert!(!darkspore.is_release_incompatible("steam-5.3.0.103"));
    assert!(darkspore.is_release_incompatible("retail-dvd-5.3.0.15"));
    assert_eq!(
        darkspore
            .steps_for_selected_fixes("steam-5.3.0.103", &["maxis_engine_60fps_tuning".to_owned()])
            .len(),
        1
    );
}

#[test]
fn checked_in_json_schema_matches_generated_schema() {
    let schema_path = repo_root().join("recipes").join("schema").join("game_recipe.schema.json");
    let on_disk = std::fs::read_to_string(&schema_path)
        .expect("recipes/schema/game_recipe.schema.json must exist")
        .replace("\r\n", "\n");
    let generated = ogsc_recipes::generate_json_schema_pretty()
        .expect("schema generation should succeed")
        .replace("\r\n", "\n");

    assert_eq!(
        on_disk, generated,
        "recipes/schema/game_recipe.schema.json is out of sync; regenerate via `cargo run -p ogsc-cli -- export-schema --output recipes/schema/game_recipe.schema.json`"
    );
}
