use super::mini_client_fixture::{LEAVE_TARGET, OWNED_SKIN_ID};
use super::*;

const SELECT_SKIN: &str = "/lol-champ-select/v1/session/my-selection";
const REROLL: &str = "/lol-champ-select/v1/session/my-selection/reroll";
const SWITCH: &str = "[data-action='pause-auto-select']";
const LEAVE: &str = "[data-action='leave-game']";
const CANCEL_LEAVE: &str = ".n-popconfirm__action button:nth-child(1)";
const CONFIRM_LEAVE: &str = ".n-popconfirm__action button:nth-child(2)";

pub(super) async fn check_optional_actions(aux: &WebviewWindow, server: &MiniClient) {
    let initial_writes = server.scene.lock().unwrap().writes.len();
    assert_eq!(
        server.scene.lock().unwrap().champion,
        103,
        "begin with Ahri selected"
    );

    click(aux, "[data-action='select-skin'] .n-base-selection").await;
    check_renderer(
        aux,
        r#"(() => {
            const options = [...document.querySelectorAll('.n-base-select-option')];
            const owned = options.find(option => option.textContent.trim() === 'Fixture owned Ahri');
            return !!owned && owned.querySelector('img')?.naturalWidth === 3 &&
              options.some(option => option.textContent.trim() === 'Fixture base Ahri') &&
              !options.some(option => option.textContent.includes('Fixture locked Ahri'));
        })()"#,
        "owned skin choices, decoded local preview and no locked option",
        || {},
    )
    .await;
    aux.eval(
        r#"[...document.querySelectorAll('.n-base-select-option')]
            .find(option => option.textContent.trim() === 'Fixture owned Ahri').click()"#,
    )
    .expect("click the owned Ahri skin once");
    check_renderer(
        aux,
        r#"document.querySelector('[data-action="select-skin"] .n-base-selection-label')
            ?.textContent.includes('Fixture owned Ahri') === true"#,
        "skin selection read back from the fake LCU",
        || {},
    )
    .await;
    assert_eq!(server.scene.lock().unwrap().selected_skin, OWNED_SKIN_ID);
    assert_writes_since(server, initial_writes, &[("PATCH", SELECT_SKIN)]);

    click(aux, "[data-action='reroll']").await;
    check_renderer(
        aux,
        r#"document.querySelector('.current-champion[data-champion-id="55"]') != null &&
            document.querySelector('.bench [data-champion-id="103"]') != null"#,
        "ordinary reroll updates the current champion and bench",
        || {},
    )
    .await;
    click(aux, "[data-action='charity-reroll']").await;
    check_renderer(
        aux,
        r#"document.querySelector('.current-champion[data-champion-id="55"]') != null &&
            document.querySelector('.bench [data-champion-id="22"]') != null"#,
        "charity reroll safely regains the original champion",
        || {},
    )
    .await;
    {
        let scene = server.scene.lock().unwrap();
        assert_eq!(
            (scene.champion, scene.bench, scene.rerolls_remaining),
            (55, 22, 0)
        );
    }
    let selection_writes = [
        ("PATCH", SELECT_SKIN),
        ("POST", REROLL),
        ("POST", REROLL),
        ("POST", "/lol-champ-select/v1/session/bench/swap/55"),
    ];
    assert_writes_since(server, initial_writes, &selection_writes);

    for paused in [true, false] {
        click(aux, SWITCH).await;
        check_renderer(
            aux,
            &format!(
                "document.querySelector({})?.getAttribute('aria-checked') === '{}'",
                json!(SWITCH),
                paused
            ),
            "auto-select pause state returned by native IPC",
            || {},
        )
        .await;
    }
    assert_writes_since(server, initial_writes, &selection_writes);

    click(aux, LEAVE).await;
    click(aux, CANCEL_LEAVE).await;
    check_renderer(
        aux,
        "!document.querySelector('.n-popconfirm__action button')?.getClientRects().length",
        "canceling the leave confirmation closes it without an LCU request",
        || {},
    )
    .await;
    assert_writes_since(server, initial_writes, &selection_writes);
    click(aux, LEAVE).await;
    click(aux, CONFIRM_LEAVE).await;
    let deadline = Instant::now() + OPEN_TIMEOUT;
    while server.scene.lock().unwrap().writes.len() == initial_writes + selection_writes.len() {
        assert!(
            Instant::now() < deadline,
            "confirmed leave did not reach the fake LCU"
        );
        tokio::time::sleep(POLL).await;
    }
    check_renderer(
        aux,
        "!document.querySelector('.n-popconfirm__action button')?.getClientRects().length",
        "the confirmed leave received a successful LCU response",
        || {},
    )
    .await;
    // The fake phase deliberately remains ChampSelect, so a retry loop would send extra requests.
    tokio::time::sleep(POLL * 4).await;
    let mut expected = selection_writes.to_vec();
    expected.push(("POST", LEAVE_TARGET));
    assert_writes_since(server, initial_writes, &expected);
    println!("Win32/WebView2 Mini optional controls: owned skin preview and change, ordinary and charity rerolls, native pause/resume, canceled leave and one confirmed leave verified.");
}

fn assert_writes_since(server: &MiniClient, offset: usize, expected: &[(&str, &str)]) {
    let scene = server.scene.lock().unwrap();
    let observed: Vec<_> = scene.writes[offset..]
        .iter()
        .map(|(method, path)| (method.as_str(), path.as_str()))
        .collect();
    assert_eq!(observed.as_slice(), expected);
}
