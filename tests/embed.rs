//! Needs the model in ~/.engram/models and llama-server; run with `cargo test -- --ignored`.

use engram::embed;

#[test]
#[ignore]
fn similar_texts_are_closer() {
    let v = embed::embed(&[
        "zsh не ділить $VAR на слова",
        "в zsh змінна не розбивається на слова",
        "рецепт борщу",
    ])
    .unwrap();
    assert!(v.iter().all(|x| x.len() == embed::DIM));
    let norm = embed::cosine(&v[0], &v[0]);
    assert!((norm - 1.0).abs() < 1e-3, "vectors are normalised");
    assert!(embed::cosine(&v[0], &v[1]) > embed::cosine(&v[0], &v[2]) + 0.2);
}
