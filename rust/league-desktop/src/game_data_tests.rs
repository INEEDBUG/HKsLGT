use super::*;
use serde_json::json;

#[test]
fn projects_the_catalogs_into_the_pinned_vue_store_shape() {
    use GameDataEndpoint::*;
    let mut read = GameDataRead::default();
    for (endpoint, payload) in [
        (
            Champions,
            json!([
                {"id": -1, "name": "无", "alias": "None", "roles": []},
                {"id": 22, "name": "艾希", "alias": "Ashe", "roles": ["marksman"],
                 "squarePortraitPath": "/lol-game-data/assets/v1/champion-icons/22.png",
                 "futureField": "unused"}
            ]),
        ),
        (
            Perks,
            json!([{"id": 8005, "name": "强攻", "longDesc": "<b>加成</b><br>",
                "endOfGameStatDescs": ["额外伤害：@f1@"]}]),
        ),
        (
            Perkstyles,
            json!({"schemaVersion": 2, "styles": [{"id": 8000, "name": "精密",
                "allowedSubStyles": [8400], "slots": [{"type": "kKeyStone", "perks": [8005]}],
                "assetMap": {"svg_icon": "/lol-game-data/assets/v1/perk-images/style.svg"}}]}),
        ),
        (
            SummonerSpells,
            json!([
                {"id": 32, "name": "标记", "cooldown": 80.0, "gameModes": ["ARAM", "KIWI"]},
                {"id": 4294967295_u32, "name": "原初惩戒", "gameModes": []}
            ]),
        ),
        (
            Items,
            json!([{"id": 1001, "name": "速度之靴", "price": 300, "priceTotal": 300,
                "from": [], "to": [3006], "description": "<mainText><stats>移速</stats></mainText>"}]),
        ),
        (
            Augments,
            json!([{"id":1133,"nameTRA":"魔法飞弹","rarity":"kGold",
                "augmentSmallIconPath":"/lol-game-data/assets/ASSETS/Maps/Particles/Kiwi/MagicMissile_small.png"}]),
        ),
        (Queues, json!([{"id": 450, "name": "极地大乱斗"}])),
    ] {
        read.record(endpoint, Ok(Some(payload))).unwrap();
    }
    assert!(read.unavailable.is_empty());
    assert_eq!(read.fields.len(), 7);
    assert_eq!(read.fields["champions"]["-1"]["id"], -1);
    assert_eq!(read.fields["champions"]["22"]["name"], "艾希");
    assert!(read.fields["champions"]["22"].get("futureField").is_none());
    assert_eq!(read.fields["perks"]["8005"]["longDesc"], "<b>加成</b><br>");
    assert_eq!(read.fields["perkstyles"]["schemaVersion"], 2);
    assert_eq!(
        read.fields["perkstyles"]["styles"]["8000"]["slots"][0]["perks"],
        json!([8005])
    );
    assert_eq!(
        read.fields["summonerSpells"]["32"]["gameModes"],
        json!(["ARAM", "KIWI"])
    );
    assert_eq!(
        read.fields["summonerSpells"]["4294967295"]["name"],
        "原初惩戒"
    );
    assert_eq!(read.fields["items"]["1001"]["to"], json!([3006]));
    assert_eq!(read.fields["augments"]["1133"]["nameTRA"], "魔法飞弹");
    assert_eq!(read.fields["augments"]["1133"]["rarity"], "kGold");
    assert!(read.fields["augments"]["1133"]["augmentSmallIconPath"]
        .as_str()
        .is_some_and(|path| path.ends_with("/MagicMissile_small.png")));
    assert_eq!(read.fields["queues"]["450"]["name"], "极地大乱斗");
}

#[test]
fn supports_legacy_queue_maps_and_keeps_the_first_zero_id() {
    let modern = GameDataEndpoint::Queues
        .normalize(json!([
            {"id": 0, "name": "自定义"},
            {"id": 0, "name": "后来出现的零 ID"},
            {"id": 450, "name": "极地大乱斗"}
        ]))
        .unwrap();
    assert_eq!(modern["0"]["name"], "自定义");

    let legacy = GameDataEndpoint::Queues
        .normalize(json!({
            "450": {"name": "极地大乱斗", "shortName": "乱斗"}
        }))
        .unwrap();
    assert_eq!(
        legacy["450"],
        json!({"id": 450, "name": "极地大乱斗", "shortName": "乱斗"})
    );
    for value in [
        json!({"__proto__": {"name": "Invalid"}}),
        json!({"0450": {"name": "Invalid"}}),
        json!({"450": {"id": 900, "name": "Invalid"}}),
    ] {
        assert_eq!(
            GameDataEndpoint::Queues.normalize(value),
            Err(LcuError::InvalidPayload)
        );
    }
}

#[test]
fn rejects_bad_table_shapes_without_discarding_a_successful_catalog() {
    use GameDataEndpoint::*;
    let mut read = GameDataRead::default();
    read.record(Champions, Ok(Some(json!([{"id": 22, "name": "艾希"}]))))
        .unwrap();
    read.record(
        Items,
        Ok(Some(
            json!([{"id": 1001, "name": "物品", "from": ["1002"]}]),
        )),
    )
    .unwrap();
    read.record(Perks, Ok(None)).unwrap();
    read.record(Queues, Err(LcuError::ResponseTooLarge))
        .unwrap();
    assert_eq!(read.fields["champions"]["22"]["name"], "艾希");
    assert_eq!(read.fields.len(), 1);
    assert_eq!(
        read.unavailable,
        vec![
            (Items, GameDataReadError::Read(LcuError::InvalidPayload)),
            (Perks, GameDataReadError::Missing),
            (Queues, GameDataReadError::Read(LcuError::ResponseTooLarge)),
        ]
    );
    assert_eq!(
        read.record(Perkstyles, Err(LcuError::AuthenticationRejected)),
        Err(LcuError::AuthenticationRejected)
    );
    assert_eq!(
        Perkstyles.normalize(json!({"schemaVersion": "2", "styles": []})),
        Err(LcuError::InvalidPayload)
    );
    assert!(GameDataReadError::Missing.can_retry());
    assert!(GameDataReadError::Read(LcuError::UnexpectedStatus(503)).can_retry());
    assert!(!GameDataReadError::Read(LcuError::InvalidPayload).can_retry());
    let oversized = Value::Array(vec![json!({"id": 1, "name": "Spell"}); 129]);
    assert_eq!(
        SummonerSpells.normalize(oversized),
        Err(LcuError::InvalidPayload)
    );
}

#[test]
fn discards_a_malformed_icon_record_without_losing_the_rest_of_its_catalog() {
    let items = GameDataEndpoint::Items
        .normalize(json!([
            {"id": 1001, "name": "速度之靴", "price": 300, "from": [], "to": [3006]},
            {"id": "invalid", "name": "损坏条目"},
            {"id": 3006, "name": "狂战士胫甲", "price": 1100, "from": [1001], "to": []},
            {"id": 3006, "name": "损坏的重复条目", "from": ["1001"], "to": []},
        ]))
        .unwrap();
    assert_eq!(items.as_object().unwrap().len(), 2);
    assert_eq!(items["1001"]["price"], 300);
    assert_eq!(items["3006"]["from"], json!([1001]));

    assert_eq!(
        GameDataEndpoint::Queues.normalize(json!([{"id": 450, "name": "ARAM"}, {"id": -2}])),
        Err(LcuError::InvalidPayload)
    );
}

#[test]
fn retries_empty_icon_catalogs_without_publishing_them_as_complete() {
    use GameDataEndpoint::*;
    let mut read = GameDataRead::default();
    read.record(Items, Ok(Some(json!([])))).unwrap();
    read.record(Perkstyles, Ok(Some(json!({"schemaVersion": 2, "styles": []}))))
        .unwrap();
    assert!(read.fields.is_empty());
    assert_eq!(
        read.unavailable,
        vec![(Items, GameDataReadError::Missing), (Perkstyles, GameDataReadError::Missing)]
    );
}

#[test]
fn retains_tooltip_formatting_with_inert_markup_and_local_icon_paths() {
    let data = GameDataEndpoint::Perks.normalize(json!([{
        "id": 1,
        "name": "测试",
        "iconPath": "//example.invalid/p.png",
        "longDesc": "<mainText>内容<br/><attention>+1</attention><jadeUnique>限购</jadeUnique><lol-uikit-tooltipped-keyword data-label='a'>独特</lol-uikit-tooltipped-keyword><img src=x onerror=a></mainText>",
        "endOfGameStatDescs": ["<a href='https://example.invalid'>统计</a>", "低于 < 5 点"]
    }])).unwrap();
    assert!(data["1"].get("iconPath").is_none());
    assert_eq!(
        data["1"]["longDesc"],
        "<mainText>内容<br/><attention>+1</attention>限购<lol-uikit-tooltipped-keyword>独特</lol-uikit-tooltipped-keyword></mainText>"
    );
    assert_eq!(
        data["1"]["endOfGameStatDescs"],
        json!(["统计", "低于 &lt; 5 点"])
    );
}

#[test]
fn reuses_the_verified_lcu_origin_and_bounded_get_routes() {
    use GameDataEndpoint::*;
    let transport = LcuTransport::new().unwrap();
    let target = LcuTarget::new(31111, "fixture-only").unwrap();
    for (endpoint, path, max_kib) in [
        (Champions, "champion-summary", 256),
        (Perks, "perks", 512),
        (Perkstyles, "perkstyles", 128),
        (SummonerSpells, "summoner-spells", 128),
        (Items, "items", 2048),
        (Augments, "cherry-augments", 512),
        (Queues, "queues", 1024),
    ] {
        let route = ReadEndpoint::GameData(endpoint);
        let request = transport.request(&target, route).build().unwrap();
        assert_eq!(request.method(), reqwest::Method::GET);
        assert_eq!(
            request.url().as_str(),
            format!("https://127.0.0.1:31111/lol-game-data/assets/v1/{path}.json")
        );
        assert!(request.headers()[reqwest::header::AUTHORIZATION].is_sensitive());
        assert_eq!(route.body_limit(), max_kib * 1024);
    }
}
