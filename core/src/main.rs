//! Omni search engine — entry point.
//!
//! Builds (or loads) a positional inverted index and serves the two HTTP
//! endpoints Flux needs (`/search`, `/ac`). Sources: the crawler's doc store
//! (`--docs`) and/or local HTML (`--corpus`); `--index <file>` persists the
//! built index and loads it on later startups. See PLAN.md.
//!
//! Usage:
//!   omni [--docs <dir>] [--corpus <dir>] [--index <dir>] [--addr <host:port>]
//! Defaults: --corpus ../corpus  --addr 0.0.0.0:8080 (reachable from a Windows host over WSL)

mod analyze;
mod bangs;
mod cards;
mod corpus;
mod docstore;
mod embed;
mod hnsw;
mod index;
mod json;
mod live;
mod merge;
mod mmap;
mod pagerank;
mod passages;
mod persist;
mod query;
mod rag;
mod rerank;
mod score;
mod segment;
mod server;
mod snippet;
mod spell;
mod suggest;
mod telemetry;
mod wand;

use std::path::PathBuf;

fn main() {
    // Sources: `--docs <dir>` reads the crawler's doc store (Phase 2);
    // `--corpus <dir>` reads raw local `.html` files (Phase 1). Both may be
    // given; if neither is, default to the local corpus.
    let mut docs_dir: Option<PathBuf> = None;
    let mut corpus_dir: Option<PathBuf> = None;
    let mut index_path: Option<PathBuf> = None;
    let mut update_dir: Option<PathBuf> = None;
    let mut embed_spec: Option<String> = None;
    let mut embed_model: Option<String> = None;
    let mut mmap_load = false;
    let mut no_serve = false;
    let mut compact = false;
    let mut auto_merge = true;
    let mut merge_factor: usize = 10;
    let mut bg_merge_secs: u64 = 30;
    let mut essential = false;
    let mut ann_mmap = false;
    let mut addr = String::from("0.0.0.0:8080");

    // Tiny hand-rolled arg parser (no clap dependency).
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--docs" if i + 1 < args.len() => {
                docs_dir = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--corpus" if i + 1 < args.len() => {
                corpus_dir = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--index" if i + 1 < args.len() => {
                index_path = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--update" if i + 1 < args.len() => {
                update_dir = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--embed" if i + 1 < args.len() => {
                embed_spec = Some(args[i + 1].clone());
                i += 2;
            }
            "--embed-model" if i + 1 < args.len() => {
                embed_model = Some(args[i + 1].clone());
                i += 2;
            }
            "--mmap" => {
                mmap_load = true;
                i += 1;
            }
            "--no-serve" => {
                no_serve = true;
                i += 1;
            }
            "--compact" => {
                compact = true;
                i += 1;
            }
            "--no-merge" => {
                auto_merge = false;
                i += 1;
            }
            "--essential" => {
                essential = true;
                i += 1;
            }
            "--ann-mmap" => {
                ann_mmap = true;
                i += 1;
            }
            "--merge-factor" if i + 1 < args.len() => {
                match args[i + 1].parse::<usize>() {
                    Ok(f) if f >= 2 => merge_factor = f,
                    _ => {
                        eprintln!("omni: --merge-factor must be an integer ≥ 2");
                        std::process::exit(2);
                    }
                }
                i += 2;
            }
            "--bg-merge-secs" if i + 1 < args.len() => {
                match args[i + 1].parse::<u64>() {
                    Ok(s) => bg_merge_secs = s,
                    _ => {
                        eprintln!("omni: --bg-merge-secs must be a non-negative integer");
                        std::process::exit(2);
                    }
                }
                i += 2;
            }
            "--addr" if i + 1 < args.len() => {
                addr = args[i + 1].clone();
                i += 2;
            }
            "--help" | "-h" => {
                println!(
                    "omni [--docs <dir>] [--corpus <dir>] [--index <dir>] \
                     [--update <store>] [--addr <host:port>]\n\n  \
                     --index <dir>    load the prebuilt index directory if it exists; otherwise \
                     build from the source(s) and save it there.\n  \
                     --update <store> incrementally apply a doc store to --index (add/replace/\
                     delete changed pages), then save. Builds fresh if no index exists yet.\n  \
                     --embed <spec>   enable semantic/hybrid search. spec: off | hash | hash:DIM \
                     | ollama | http://host:port/api/embeddings  (model via --embed-model).\n  \
                     --embed-model <name>  embedding model for the http/ollama embedder.\n  \
                     --mmap           load the prebuilt --index via a memory map (no heap copy).\n  \
                     --no-serve       build/update/save the index and exit (don't start the server).\n  \
                     --compact        merge segments into one, dropping tombstones (then save).\n  \
                     --merge-factor N tiered auto-merge: combine a size tier once it holds N+ \
                     segments (default 10).\n  \
                     --no-merge       disable the automatic tiered merge after an update.\n  \
                     --bg-merge-secs N  while serving, run the tiered merge in the background every \
                     N seconds with atomic index swaps (default 30; 0 disables).\n  \
                     --essential      add curated essential-site launch cards (YouTube, GitHub, \
                     Overleaf, …) to the index. !bang shortcuts work regardless.\n  \
                     --ann-mmap       keep no RAM copy of ANN vectors — decode them from the \
                     segments on demand (less memory at scale, slower queries)."
                );
                return;
            }
            other => {
                eprintln!("omni: unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let has_source = docs_dir.is_some() || corpus_dir.is_some();

    // Choose how to load a prebuilt index: a plain read, or a memory map.
    let load_index: fn(&std::path::Path) -> std::io::Result<index::Index> = if mmap_load {
        persist::load_mmap
    } else {
        persist::load
    };

    // Incremental update path: apply a doc store to an existing index in place.
    let mut idx = if let Some(udir) = update_dir.as_ref() {
        match index_path.as_ref().filter(|p| p.exists()) {
            Some(path) => {
                let mut idx = match load_index(path) {
                    Ok(i) => i,
                    Err(e) => {
                        eprintln!("omni: could not load index {}: {e}", path.display());
                        std::process::exit(1);
                    }
                };
                match docstore::update(&mut idx, udir) {
                    Ok(s) => println!(
                        "omni: update — +{} added, ~{} changed, -{} deleted, {} unchanged ({} live docs)",
                        s.added, s.changed, s.deleted, s.unchanged, idx.doc_count()
                    ),
                    Err(e) => {
                        eprintln!("omni: update failed for {}: {e}", udir.display());
                        std::process::exit(1);
                    }
                }
                // Tiered auto-merge so repeated updates don't pile up segments.
                if auto_merge {
                    let n = idx.maybe_merge(&merge::MergePolicy::new(merge_factor));
                    if n > 0 {
                        println!(
                            "omni: auto-merged in {n} step(s) → {} segment(s)",
                            idx.segment_count()
                        );
                    }
                }
                if let Some(path) = &index_path {
                    if let Err(e) = persist::save(&idx, path) {
                        eprintln!(
                            "omni: warning: could not save index {}: {e}",
                            path.display()
                        );
                    } else {
                        println!("omni: saved index to {}", path.display());
                    }
                }
                idx
            }
            None => {
                // No existing index → first build from the store.
                let mut idx = index::Index::new();
                match docstore::load_dir(&mut idx, udir) {
                    Ok(n) => println!("omni: built index from {n} doc(s) in {}", udir.display()),
                    Err(e) => {
                        eprintln!("omni: could not read doc store {}: {e}", udir.display());
                        std::process::exit(1);
                    }
                }
                if let Some(path) = &index_path {
                    match persist::save(&idx, path) {
                        Ok(()) => println!("omni: saved index to {}", path.display()),
                        Err(e) => eprintln!(
                            "omni: warning: could not save index {}: {e}",
                            path.display()
                        ),
                    }
                }
                idx
            }
        }
    }
    // Fast path: a prebuilt index exists and no source was given → just load it.
    else if let Some(path) = index_path.as_ref().filter(|p| p.exists() && !has_source) {
        match load_index(path) {
            Ok(idx) => {
                println!(
                    "omni: loaded prebuilt index ({} docs) from {}",
                    idx.doc_count(),
                    path.display()
                );
                idx
            }
            Err(e) => {
                eprintln!("omni: could not load index {}: {e}", path.display());
                std::process::exit(1);
            }
        }
    } else {
        // Build from sources. Default to the local corpus if none specified.
        if !has_source {
            corpus_dir = Some(PathBuf::from("../corpus"));
        }
        let mut idx = index::Index::new();
        if let Some(dir) = &docs_dir {
            match docstore::load_dir(&mut idx, dir) {
                Ok(n) => println!("omni: indexed {n} crawled doc(s) from {}", dir.display()),
                Err(e) => {
                    eprintln!("omni: could not read doc store {}: {e}", dir.display());
                    std::process::exit(1);
                }
            }
        }
        if let Some(dir) = &corpus_dir {
            match corpus::load_dir(&mut idx, dir) {
                Ok(n) => println!("omni: indexed {n} local doc(s) from {}", dir.display()),
                Err(e) => {
                    eprintln!("omni: could not read corpus dir {}: {e}", dir.display());
                    std::process::exit(1);
                }
            }
        }
        // Persist the freshly-built index for next startup.
        if let Some(path) = &index_path {
            match persist::save(&idx, path) {
                Ok(()) => println!("omni: saved index to {}", path.display()),
                Err(e) => eprintln!(
                    "omni: warning: could not save index {}: {e}",
                    path.display()
                ),
            }
        }
        idx
    };

    // Essential-site launch cards (necessity category): YouTube, GitHub, Overleaf,
    // … added as searchable index entries. The `!bang` shortcuts work regardless.
    if essential {
        let added = idx.add_essential_sites();
        if added > 0 {
            println!("omni: added {added} essential-site launch card(s)");
            if let Some(p) = &index_path {
                if let Err(e) = persist::save(&idx, p) {
                    eprintln!("omni: warning: could not save essential cards: {e}");
                }
            }
        }
    }

    // Compaction: merge segments into one and reclaim tombstoned space.
    if compact {
        let before = idx.segment_count();
        idx.compact();
        println!(
            "omni: compacted {before} segment(s) → 1 ({} live docs)",
            idx.doc_count()
        );
        if let Some(path) = &index_path {
            if let Err(e) = persist::save(&idx, path) {
                eprintln!(
                    "omni: warning: could not save compacted index {}: {e}",
                    path.display()
                );
            }
        }
    }

    // Semantic embeddings: enable from --embed, or reuse the index's stored
    // config so a plain reload keeps hybrid search on. Embeds only docs missing a
    // vector (so incremental updates embed just the new pages), and re-saves.
    let emb_cfg = match &embed_spec {
        Some(spec) => match embed::EmbedderConfig::parse(spec, embed_model.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("omni: {e}");
                std::process::exit(2);
            }
        },
        None => idx.embedder().clone(),
    };
    if emb_cfg.enabled() {
        let existing = idx.embedder().clone();
        let switched = existing.enabled()
            && (existing.kind != emb_cfg.kind
                || existing.url != emb_cfg.url
                || existing.model != emb_cfg.model);
        if switched {
            idx.clear_embeddings(); // different embedder → vectors are incomparable
        }
        idx.set_embedder(emb_cfg.clone());
        if let Some(e) = embed::Embedder::from_config(&emb_cfg) {
            let n = idx.embed_missing(&e);
            if n > 0 {
                println!(
                    "omni: embedded {n} doc(s) — semantic/hybrid search ON (dim {})",
                    idx.embedder().dim
                );
                if let Some(path) = &index_path {
                    if let Err(e) = persist::save(&idx, path) {
                        eprintln!(
                            "omni: warning: could not save embeddings to {}: {e}",
                            path.display()
                        );
                    }
                }
            } else if idx.doc_count() > 0
                && idx
                    .segments()
                    .iter()
                    .flat_map(|s| s.docs.iter())
                    .any(|d| !d.deleted && d.emb_len > 0)
            {
                println!("omni: semantic/hybrid search ON (already embedded)");
            } else {
                eprintln!(
                    "omni: warning: --embed set but nothing embedded (is the embedder reachable?)"
                );
            }
        }
    }

    // Approximate-NN graph for sub-linear semantic recall. Reuse a persisted
    // graph if its signature still matches the index; otherwise (re)build and save
    // it. No-op if the corpus is too small or unembedded (search then uses exact
    // brute force).
    idx.set_ann_lazy(ann_mmap); // lazy graph (no RAM vector copy) when --ann-mmap
    let ann_loaded = index_path
        .as_ref()
        .map(|p| persist::load_ann(&mut idx, p))
        .unwrap_or(false);
    if ann_loaded {
        if let Some(ann) = idx.ann() {
            let mode = if ann.is_lazy() { " (lazy/mmap)" } else { "" };
            println!(
                "omni: loaded HNSW ANN ({} vectors){mode} from disk",
                ann.len()
            );
        }
    } else {
        idx.build_ann();
        if let Some(ann) = idx.ann() {
            println!(
                "omni: built HNSW ANN over {} vectors — semantic recall is sub-linear",
                ann.len()
            );
            if let Some(p) = &index_path {
                if let Err(e) = persist::save_ann(&idx, p) {
                    eprintln!("omni: warning: could not save ANN sidecar: {e}");
                }
            }
        }
    }

    if no_serve {
        println!(
            "omni: index ready ({} live docs) — not serving (--no-serve)",
            idx.doc_count()
        );
        return;
    }

    // Serve through a hot-swappable handle so a background merge can rebalance the
    // index atomically while requests are in flight.
    let live = live::LiveIndex::new(idx);
    // Background merge is independent of the offline `--no-merge` switch: the
    // former rebalances the *running* index, the latter the build/update path.
    let bg = (bg_merge_secs > 0).then(|| live::BgMerge {
        interval: std::time::Duration::from_secs(bg_merge_secs),
        merge_factor,
        dir: index_path.clone(),
    });
    if let Err(e) = server::serve(live, &addr, bg, index_path) {
        eprintln!("omni: server error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_index() -> index::Index {
        let mut idx = index::Index::new();
        idx.add_document(
            "a".into(),
            "Rust Lang".into(),
            "rust is a systems programming language",
        );
        idx.add_document(
            "b".into(),
            "Go Lang".into(),
            "go is a language for concurrent network services",
        );
        idx.add_document(
            "c".into(),
            "Rust Search".into(),
            "building a search engine in rust rust rust",
        );
        idx
    }

    #[test]
    fn ranks_relevant_docs_first() {
        let idx = sample_index();
        let hits = query::search(&idx, "rust", 10);
        assert!(!hits.is_empty());
        // Doc "c" mentions rust three times → should outrank doc "a".
        assert_eq!(hits[0].title, "Rust Search");
    }

    #[test]
    fn unknown_term_returns_nothing() {
        let idx = sample_index();
        let hits = query::search(&idx, "kubernetes", 10);
        assert!(hits.is_empty());
    }

    #[test]
    fn multi_term_query_accumulates() {
        let idx = sample_index();
        let hits = query::search(&idx, "language services", 10);
        // Doc "b" contains both query terms → ranks first.
        assert_eq!(hits[0].title, "Go Lang");
    }

    #[test]
    fn phrase_query_requires_adjacency() {
        let mut idx = index::Index::new();
        // Both contain "search" and "engine", but only "a" has them adjacent.
        idx.add_document("a".into(), "A".into(), "a fast search engine for the web");
        idx.add_document("b".into(), "B".into(), "search the engine room of the ship");
        let phrase = query::search(&idx, "\"search engine\"", 10);
        assert_eq!(phrase.len(), 1);
        assert_eq!(phrase[0].title, "A");
        // Without quotes, both match (free terms).
        assert_eq!(query::search(&idx, "search engine", 10).len(), 2);
    }

    #[test]
    fn bm25f_weights_title_over_body() {
        // Same term, same frequency — but doc "a" has it in the title field and
        // doc "b" in the body. BM25F's title weight should rank "a" first.
        let mut idx = index::Index::new();
        idx.add_document(
            "a".into(),
            "Photosynthesis".into(),
            "an unrelated body about weather",
        );
        idx.add_document(
            "b".into(),
            "Weather report".into(),
            "a long note that mentions photosynthesis once",
        );
        let hits = query::search(&idx, "photosynthesis", 10);
        assert_eq!(hits[0].url, "a", "a title hit should outrank a body hit");
    }

    #[test]
    fn hybrid_retrieval_with_embeddings() {
        let mut idx = index::Index::new();
        idx.add_document(
            "a".into(),
            "Rust".into(),
            "rust ownership and borrowing rules",
        );
        idx.add_document("b".into(), "Pasta".into(), "italian tomato pasta recipe");
        let cfg = embed::EmbedderConfig::parse("hash", None).unwrap();
        idx.set_embedder(cfg.clone());
        let e = embed::Embedder::from_config(&cfg).unwrap();
        assert_eq!(idx.embed_missing(&e), 2);

        // With hybrid (lexical ⊕ semantic) active, the relevant doc still leads.
        let hits = query::search(&idx, "ownership", 10);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].url, "a");
    }

    #[test]
    fn stemming_matches_morphological_variants() {
        let mut idx = index::Index::new();
        idx.add_document(
            "a".into(),
            "Doc".into(),
            "the system was connecting to peers",
        );
        // Query "connection" should find the doc that says "connecting".
        let hits = query::search(&idx, "connection", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "a");
    }

    #[test]
    fn stop_words_do_not_create_matches() {
        let mut idx = index::Index::new();
        idx.add_document("a".into(), "Doc".into(), "the cat sat on the mat");
        // "the" is a stop-word → not indexed → a query of only stop-words finds nothing.
        assert!(query::search(&idx, "the", 10).is_empty());
        assert_eq!(query::search(&idx, "cat", 10).len(), 1);
    }

    #[test]
    fn proximity_rewards_clustered_terms() {
        let mut idx = index::Index::new();
        // Both contain "quantum" and "computing"; doc "a" has them adjacent.
        idx.add_document(
            "a".into(),
            "A".into(),
            "research into quantum computing breakthroughs",
        );
        idx.add_document(
            "b".into(),
            "B".into(),
            "quantum mechanics is hard and unrelated computing topics appear far later here",
        );
        let hits = query::search(&idx, "quantum computing", 10);
        assert_eq!(hits[0].url, "a", "adjacent terms should win on proximity");
    }

    #[test]
    fn title_match_boosts_ranking() {
        let mut idx = index::Index::new();
        // Doc "a" mentions rust more in body; doc "b" has it in the title.
        idx.add_document("a".into(), "Systems".into(), "rust rust programming notes");
        idx.add_document("b".into(), "Rust".into(), "a short note about programming");
        let hits = query::search(&idx, "rust", 10);
        // The title hit pushes "b" to the top despite lower body frequency.
        assert_eq!(hits[0].title, "Rust");
    }
}
