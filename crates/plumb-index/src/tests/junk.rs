//! Sign-in pages, gambling spam, app installers, fake help lines and
//! abuse contacts of sites a search does not name are left out, and so
//! are domains only held.

use super::*;
use crate::junk::{
    is_abuse_contact, is_gambling, is_gambling_address, is_holding_page, offers_apk,
    offers_help_line,
};

/// A site found only by meaning, whose homepage is at `url`.
fn at(domain: &str, url: &str, title: Option<&str>) -> Item {
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
        Item {
            about: Some("How workplace retirement plans work.".into()),
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
            "lazybaraustralia.example",
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
    let lazy = found("lazydocker");
    assert!(lazy.contains("lazydocker.example"), "{lazy}");
    assert!(!lazy.contains("lazybaraustralia.example"), "{lazy}");
    let lazy = found("lazy");
    assert!(!lazy.contains("312bet29.com"), "{lazy}");
    assert!(lazy.contains("lakeside.casino"), "{lazy}");
    // Asked about gambling, or named, they are found.
    assert!(found("lazy casino").contains("lazybaraustralia.example"));
    assert!(found("lazybar").contains("lazybaraustralia.example"));
    assert!(found("ebaytoto").contains("ebaytoto.example"));
    assert!(!found("ebay").contains("ebaytoto.example"));
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

#[test]
fn held_domains_are_left_out_unless_typed() {
    let records = [
        site(
            "goldenglobes.com",
            Some("Golden Globes"),
            Some("The Golden Globe Awards for film and television."),
            &["Golden Globes"],
            &[("golden globes", 300)],
            popular(3_000, 2_000),
        ),
        // The old address, kept by a brand-protection registrar and still
        // linked as the awards' site.
        site(
            "goldenglobes.org",
            Some("Registered & Protected by MarkMonitor"),
            None,
            &[],
            &[("golden globes", 200)],
            obscure(20_000, 400),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let found =
        |query: &str| domains(&searcher.search_with(query, 10, &keep_all()).unwrap()).join(" ");
    assert_eq!(found("golden globes"), "goldenglobes.com");
    assert!(found("goldenglobes.org").contains("goldenglobes.org"));
}

#[test]
fn held_domains_are_told_by_their_notices() {
    for (domain, title, description) in [
        ("goldenglobes.org", "Registered & Protected by MarkMonitor", None),
        ("mozy.com", "Domain Registered at Safenames", None),
        ("fusion-id.com", "fusion-id.com — Domain registered with JOKER.COM", None),
        ("managedomain.nl", "Domain registered - managedomain.nl", None),
        ("u.ie", "This domain is reserved", None),
        ("labubu.st", "Domain Parked", None),
        (
            "corporationcreation.com",
            "CorporationCreation.com is for sale - Premium Domain",
            None,
        ),
        (
            "kuro5hin.org",
            "kuro5hin.org",
            Some("This domain name has been registered with Gandi.net. It is currently parked by the owner."),
        ),
    ] {
        assert!(is_holding_page(domain, Some(title), description), "{title}");
    }
    for (domain, title) in [
        ("networksolutions.com", "Network Solutions"),
        ("nicnames.com", "Domain Registrar | NicNames.com"),
        (
            "registerdomain.co.za",
            "Best Domain Registration in South Africa ☑️ Register Domains",
        ),
        (
            "markmonitor.com",
            "MarkMonitor | Brand Protection and Domain Management",
        ),
        ("boats.example", "Classic Boat is for sale"),
    ] {
        assert!(!is_holding_page(domain, Some(title), None), "{title}");
    }
    let whois = "Look up who owns a domain and when this domain has been registered.";
    assert!(!is_holding_page(
        "whois.example",
        Some("WHOIS lookup"),
        Some(whois)
    ));
}

#[test]
fn app_installers_are_left_out_unless_asked_for() {
    let records = [
        site(
            "kinemaster.example",
            Some("KineMaster - Video Editor"),
            Some("Edit videos on your phone."),
            &["KineMaster"],
            &[("kinemaster", 300)],
            popular(3_000, 2_000),
        ),
        site(
            "kinemastermodapk.example",
            Some("Kinemaster Mod APK without Watermark Download 2026"),
            Some("KineMaster Mod APK lets you unlock premium features for free."),
            &[],
            &[("kinemaster", 40)],
            obscure(90_000, 30),
        ),
        // The app's own site, which hands out its installer.
        site(
            "snaptube.example",
            Some("Snaptube: Download Videos and Music for Free"),
            Some("Download the Snaptube APK for Android."),
            &[],
            &[("snaptube", 40)],
            obscure(90_000, 30),
        ),
        site(
            "snaptubes.example",
            Some("Snaptube Latest Version Download and Update"),
            Some("Download Snaptube latest version APK."),
            &[],
            &[("snaptube", 20)],
            obscure(120_000, 10),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let found =
        |query: &str| domains(&searcher.search_with(query, 10, &keep_all()).unwrap()).join(" ");
    assert_eq!(found("kinemaster"), "kinemaster.example");
    assert!(found("kinemaster mod apk").contains("kinemastermodapk.example"));
    assert_eq!(found("snaptube"), "snaptube.example");
    for text in [
        "Kinemaster MOD APK Download Latest Version Without Watermark",
        "GB WhatsApp APK Download Latest Version Updated Oct 2026",
        "Baixar Snaptube Grátis: Snaptube APK Download Atualizado",
    ] {
        assert!(offers_apk([Some(text)]), "{text:?}");
    }
    for text in [
        "APKPure",
        "Napkin folding for dinner parties",
        "Android app development",
    ] {
        assert!(!offers_apk([Some(text)]), "{text:?}");
    }
}

#[test]
fn help_lines_of_strangers_are_left_out() {
    let records = [
        site(
            "epson.com",
            Some("Epson"),
            Some("Printers, projectors and scanners."),
            &["Epson"],
            &[("epson", 300)],
            popular(400, 9_000),
        ),
        // A tech-support scam, named for the searches it waits for.
        site(
            "epsonprintersupportpro.example",
            Some("Epson Printer Support Number +1-844-932-1846 - Epson Support"),
            Some("Epson printer support phone number +1-844-932-1846 for Epson help."),
            &[],
            &[("epson printer support", 20)],
            obscure(200_000, 3),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let found =
        |query: &str| domains(&searcher.search_with(query, 10, &keep_all()).unwrap()).join(" ");
    let support = found("epson printer support");
    assert!(support.starts_with("epson.com"), "{support}");
    assert!(!support.contains("epsonprintersupportpro"), "{support}");
    // Spelled, the site is found.
    assert!(found("epsonprintersupportpro").contains("epsonprintersupportpro.example"));
}

#[test]
fn help_lines_are_told_by_their_titles() {
    for (title, description) in [
        (
            "Epson Printer Support Number +1-844-932-1846 - Epson Support",
            None,
        ),
        (
            "QuickBooks Customer Service Phone Number",
            Some("Call 1 (800) 555-0199 for help."),
        ),
    ] {
        assert!(offers_help_line(Some(title), description), "{title}");
    }
    for (title, description) in [
        // No number to call.
        ("Customer Service Numbers for 10,000 Companies", None),
        // A shop's own number, with no help line offered.
        (
            "Mac Repair Shop",
            Some("Phone 954-726-9525 for a free consultation."),
        ),
        (
            "Support Number Theory Seminar",
            Some("Talks on 12 May 2026."),
        ),
    ] {
        assert!(!offers_help_line(Some(title), description), "{title}");
    }
}

#[test]
fn abuse_contacts_of_name_servers_are_left_out() {
    let records = [
        site(
            "elderabusehelp.example",
            Some("Elder Abuse Help Line"),
            Some("Help and advice for older people facing abuse."),
            &[],
            &[("elder abuse", 40)],
            obscure(300_000, 20),
        ),
        // A name server's homepage, only an abuse contact, as nspmanaged.com
        // listed for "memantine abuse" and "elder abuse".
        site(
            "nsmanaged.example",
            Some("Abuse"),
            None,
            &[],
            &[],
            obscure(200_000, 40),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let found =
        |query: &str| domains(&searcher.search_with(query, 10, &keep_all()).unwrap()).join(" ");
    assert_eq!(found("elder abuse"), "elderabusehelp.example");
    assert_eq!(found("abuse"), "elderabusehelp.example");
    // Spelled, the site is found.
    assert!(found("nsmanaged").contains("nsmanaged.example"));
}

#[test]
fn abuse_contacts_are_told_by_their_titles() {
    for title in [
        "Abuse",
        "Report Abuse",
        "Report Abuse - SecureDNS93.com",
        "Abuse Kontakt – mail.leenux.de",
    ] {
        assert!(is_abuse_contact(Some(title), None), "{title}");
    }
    for title in ["Elder Abuse Help Line", "Network Abuse Clearinghouse"] {
        assert!(!is_abuse_contact(Some(title), None), "{title}");
    }
    // A site that says what it does is more than a contact.
    assert!(!is_abuse_contact(
        Some("Report Abuse"),
        Some("How to report child abuse in Texas.")
    ));
}
