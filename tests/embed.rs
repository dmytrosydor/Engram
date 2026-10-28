//! Needs the model in ~/.engram/models and llama-server; run with `cargo test -- --ignored`.

use engram::embed;

#[test]
#[ignore]
fn similar_texts_are_closer() {
    let v = embed::embed(&[
        "zsh does not split $VAR into words",
        "in zsh a variable is not broken into words",
        "a borscht recipe",
    ])
    .unwrap();
    assert!(v.iter().all(|x| x.len() == embed::DIM));
    let norm = embed::cosine(&v[0], &v[0]);
    assert!((norm - 1.0).abs() < 1e-3, "vectors are normalised");
    assert!(embed::cosine(&v[0], &v[1]) > embed::cosine(&v[0], &v[2]) + 0.2);
}
