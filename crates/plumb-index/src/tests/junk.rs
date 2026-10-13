//! Sign-in pages and gambling spam of sites a search does not name are
//! left out.

use super::*;
use crate::junk::{is_gambling, is_gambling_address};

/// A site found only by meaning, whose homepage is at `url`.
fn at(domain: &str, url: &str, title: Option<&str>) -> SiteRecord {
    let mut record = site(domain, title, None, &[], &[], obscure(400_000, 3));
    record.url = Some(url.to_string());
    record
}

#[test]
fn sign_in_pages_of_sites_the_query_does_not_name_are_left_out() {
    let records = [
        site(
            "fidelity.com",
            Some("Fidelity Investments"),
            Some("Retirement plans, brokerage accounts and NetBenefits."),
            &["Fidelity Investments"],
            &[("fidelity", 300)],
            popular(400, 9_000),
        ),
        // Found by meaning, as on plumbsearch.org for "Fidelity
        // NetBenefits login" and "hulu login".
        at(
            "datahubnet.example",
            "https://datahubnet.example/",
            Some("DataHubNet - Login"),
        ),
        at("qris.example", "https://qris.example/login", Some("QRIS")),
        at(
            "jenta.example",
            "https://jenta.example/",
            Some("Sign in to your account - Jenta Tech"),
        ),
        at(
            "wealthscape.com",
            "https://www.wealthscape.com/",
            Some("Wealthscape Login"),
        ),
        SiteRecord {
            description: Some("How workplace retirement plans work.".into()),
            ..at(
                "retireguide.example",
                "https://retireguide.example/",
                Some("Workplace retirement plan guide"),
            )
        },
        // Says nothing of itself, and only meaning found it.
        at(
            "cardgate.example",
            "https://cardgate.example/",
            Some("My CardGate"),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![
        ("datahubnet.example", 0.95),
        ("sb24.example", 0.94),
        ("jenta.example", 0.93),
        ("wealthscape.com", 0.92),
        ("retireguide.example", 0.9),
        ("cardgate.example", 0.89),
    ]);
    let search = |query: &str| {
        searcher
            .search_meaning(
                query,
                10,
                &keep_all(),
                &SearchOptions::default(),
                Some(&meaning),
            )
            .unwrap()
            .hits
    };
    let hits = search("fidelity netbenefits login");
    let found = domains(&hits);
    assert!(found.contains(&"fidelity.com"), "{found:?}");
    // What meaning finds stays when it says what it is, short of other
    // sites' sign-in forms.
    assert!(found.contains(&"retireguide.example"), "{found:?}");
    for login in [
        "datahubnet.example",
        "qris.example",
        "jenta.example",
        "wealthscape.com",
        "cardgate.example",
    ] {
        assert!(!found.contains(&login), "{login} in {found:?}");
    }
    // Named, a site's own sign-in page is what was asked for.
    let hits = search("wealthscape login");
    assert_eq!(domains(&hits)[0], "wealthscape.com");
    // Not asked to sign in, meaning's finds stay.
    let hits = search("fidelity netbenefits");
    let found = domains(&hits);
    assert!(found.contains(&"cardgate.example"), "{found:?}");
    assert_eq!(top(&searcher, "datahubnet"), "datahubnet.example");
}

#[test]
fn gambling_sites_are_left_out_of_searches_not_about_gambling() {
    let records = [
        site(
            "ebay.com",
            Some("eBay"),
            Some("Buy and sell electronics, cars, fashion and collectibles."),
            &["eBay"],
            &[("ebay", 900)],
            popular(30, 9_000),
        ),
        // An expired domain, its links naming it after a big site.
        site(
            "oldartgallery.example",
            Some("Situs Toto MATARAMTOTO : Link Toto Slot Depo 5k"),
            Some("Find top offers on vacuum cleaners."),
            &[],
            &[("ebay", 400)],
            obscure(90_000, 30),
        ),
        // Named after the big site, to be found with it.
        site(
            "ebaytoto.example",
            Some("EBAYTOTO: Situs Utama Resmi Akses Login Tanpa Hambatan"),
            None,
            &[],
            &[("ebay login", 50)],
            obscure(120_000, 10),
        ),
        site(
            "lazybar.example",
            Some("Lazybar Casino 🎰 Play The Best Pokies in AU!"),
            None,
            &[],
            &[("lazydocker", 5)],
            obscure(200_000, 3),
        ),
        site(
            "lazydocker.example",
            Some("lazydocker: a terminal UI for docker"),
            None,
            &[],
            &[],
            obscure(300_000, 2),
        ),
        // No words of its own, but named like a gambling site.
        site(
            "312bet29.com",
            None,
            None,
            &[],
            &[("lazy", 5)],
            obscure(200_000, 3),
        ),
        // Named like one, but it says what it is.
        site(
            "lakeside.casino",
            Some("Lakeside Hotel"),
            Some("Rooms and a restaurant by the lake."),
            &[],
            &[("lazy", 5)],
            obscure(200_000, 3),
        ),
        site(
            "nugget.example",
            Some("Golden Nugget Las Vegas Hotel & Casino"),
            Some("Hotel rooms on Fremont Street, Las Vegas."),
            &[],
            &[],
            obscure(60_000, 40),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let found =
        |query: &str| domains(&searcher.search_with(query, 10, &keep_all()).unwrap()).join(" ");
    let ebay = found("ebay");
    assert!(ebay.starts_with("ebay.com"), "{ebay}");
    assert!(!ebay.contains("oldartgallery.example"), "{ebay}");
    let login = found("ebay login");
    assert!(!login.contains("ebaytoto.example"), "{login}");
    assert!(found("ebaytoto").contains("ebaytoto.example"));
    let lazy = found("lazydocker");
    assert!(lazy.contains("lazydocker.example"), "{lazy}");
    assert!(!lazy.contains("lazybar.example"), "{lazy}");
    let lazy = found("lazy");
    assert!(!lazy.contains("312bet29.com"), "{lazy}");
    assert!(lazy.contains("lakeside.casino"), "{lazy}");
    // Asked about gambling, or named, they are found.
    assert!(found("lazy casino").contains("lazybar.example"));
    assert!(found("lazybar").contains("lazybar.example"));
    // A casino hotel is a hotel.
    assert!(found("las vegas hotel").contains("nugget.example"));
}

#[test]
fn gambling_sites_are_told_by_their_words() {
    for text in [
        "Situs Toto MATARAMTOTO : Link Toto Slot Depo 5k Terjamin Gampang Maxwin",
        "RTP Slot: Info RTP Live Slot Pragmatic Play | Bocoran RTP Slot",
        "JurusQQ: Situs Judi Pkv Games Bandarqq Online Terpercaya 2025",
        "Komputasi Ramping RUPIAHTOTO | Navigasi Ringan Pasaran Angka",
        "TomExch — Fastest Betting Exchange",
        "Squid Gamebler: the casino game",
        "حضران بت 90 (hazaranbet90)| سایت رسمی حضرات مرجع انفجار و شرط بندی",
        "สล็อตเว็บตรง เว็บแท้ 2026 เช็กเปอร์เซ็นต์ RTP",
        "MAMBA196 | Pusat Game Slot Online Resmi Dan Terpercaya",
        "杜拜娯樂城",
    ] {
        assert!(is_gambling([Some(text)]), "{text:?}");
    }
    for text in [
        "Golden Nugget Las Vegas Hotel & Casino",
        "Toto: bathroom fixtures",
        "Judi Dench fan club",
        "Bet Exam : Home",
        "BET Networks",
        "Time slot booking for clinics",
        "Interest rates (ставки) explained",
    ] {
        assert!(!is_gambling([Some(text)]), "{text:?}");
    }
    for domain in [
        "161bet.bet",
        "312bet29.com",
        "bet365.com",
        "jalurwlatogel88.net",
        "kitakaisartoto88.net",
        "pkrslots29.vip",
        "qq88.money",
    ] {
        assert!(is_gambling_address(domain), "{domain}");
    }
    for domain in [
        "tibet2025.org",
        "alphabet1.com",
        "bet.com",
        "toto.com",
        "betterhelp.com",
        "slotmachine-museum.org",
        "slot.ws",
        "247bethesdaplumber.com",
        "judi2024.com",
    ] {
        assert!(!is_gambling_address(domain), "{domain}");
    }
}
