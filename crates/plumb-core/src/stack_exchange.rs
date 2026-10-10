//! Stack Exchange's question and answer sites besides Stack Overflow whose
//! most viewed questions make the `stackexchange` page set: the how-to
//! sites people search with whole questions ("how to unclog a drain" is a
//! Home Improvement question), each named as Stack Exchange names it.

/// One Stack Exchange site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExchangeSite {
    /// The site's host, which is also its dump's name on the Internet
    /// Archive: `diy.stackexchange.com`.
    pub domain: &'static str,
    /// What people see: "Home Improvement".
    pub name: &'static str,
    /// The site's name in Stack Exchange's API: `diy`.
    pub api: &'static str,
}

const fn site(domain: &'static str, name: &'static str, api: &'static str) -> ExchangeSite {
    ExchangeSite { domain, name, api }
}

/// The sites, English ones whose questions are asked in plain words.
/// Sites whose titles are mostly formulas (Mathematics, Physics) are left
/// out: their titles don't read as searches.
pub const SITES: &[ExchangeSite] = &[
    site("superuser.com", "Super User", "superuser"),
    site("askubuntu.com", "Ask Ubuntu", "askubuntu"),
    site("serverfault.com", "Server Fault", "serverfault"),
    site("unix.stackexchange.com", "Unix & Linux", "unix"),
    site("apple.stackexchange.com", "Ask Different", "apple"),
    site(
        "android.stackexchange.com",
        "Android Enthusiasts",
        "android",
    ),
    site("webapps.stackexchange.com", "Web Applications", "webapps"),
    site(
        "security.stackexchange.com",
        "Information Security",
        "security",
    ),
    site(
        "softwareengineering.stackexchange.com",
        "Software Engineering",
        "softwareengineering",
    ),
    site("dba.stackexchange.com", "Database Administrators", "dba"),
    site("tex.stackexchange.com", "TeX - LaTeX", "tex"),
    site(
        "electronics.stackexchange.com",
        "Electrical Engineering",
        "electronics",
    ),
    site(
        "graphicdesign.stackexchange.com",
        "Graphic Design",
        "graphicdesign",
    ),
    site("ux.stackexchange.com", "User Experience", "ux"),
    site("photo.stackexchange.com", "Photography", "photo"),
    site("gaming.stackexchange.com", "Arqade", "gaming"),
    site("diy.stackexchange.com", "Home Improvement", "diy"),
    site(
        "woodworking.stackexchange.com",
        "Woodworking",
        "woodworking",
    ),
    site(
        "gardening.stackexchange.com",
        "Gardening & Landscaping",
        "gardening",
    ),
    site("cooking.stackexchange.com", "Seasoned Advice", "cooking"),
    site(
        "mechanics.stackexchange.com",
        "Motor Vehicle Maintenance & Repair",
        "mechanics",
    ),
    site("bicycles.stackexchange.com", "Bicycles", "bicycles"),
    site(
        "outdoors.stackexchange.com",
        "The Great Outdoors",
        "outdoors",
    ),
    site("fitness.stackexchange.com", "Physical Fitness", "fitness"),
    site("pets.stackexchange.com", "Pets", "pets"),
    site("parenting.stackexchange.com", "Parenting", "parenting"),
    site("travel.stackexchange.com", "Travel", "travel"),
    site(
        "expatriates.stackexchange.com",
        "Expatriates",
        "expatriates",
    ),
    site(
        "money.stackexchange.com",
        "Personal Finance & Money",
        "money",
    ),
    site("law.stackexchange.com", "Law", "law"),
    site("workplace.stackexchange.com", "The Workplace", "workplace"),
    site(
        "english.stackexchange.com",
        "English Language & Usage",
        "english",
    ),
    site("ell.stackexchange.com", "English Language Learners", "ell"),
];

/// The site with host `domain`, `None` for any other.
pub fn site_of(domain: &str) -> Option<&'static ExchangeSite> {
    SITES.iter().find(|site| site.domain == domain)
}

impl ExchangeSite {
    /// The site's whole dump on the Internet Archive, its `Posts.xml`
    /// among other files.
    pub fn dump_url(&self) -> String {
        format!(
            "https://archive.org/download/stackexchange/{}.7z",
            self.domain
        )
    }

    /// The address of question `id`.
    pub fn question_url(&self, id: u64) -> String {
        format!("https://{}/questions/{id}", self.domain)
    }
}

/// A question's item in the set's file, `diy.stackexchange.com/12345`.
pub fn question_item(site: &ExchangeSite, id: u64) -> String {
    format!("{}/{id}", site.domain)
}

/// The site and question number of an item written by [`question_item`];
/// `None` for another site or an item that does not read.
pub fn parse_question_item(item: &str) -> Option<(&'static ExchangeSite, u64)> {
    let (domain, id) = item.split_once('/')?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((site_of(domain)?, id.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_read_back() {
        let diy = site_of("diy.stackexchange.com").unwrap();
        assert_eq!(diy.name, "Home Improvement");
        let item = question_item(diy, 12345);
        assert_eq!(item, "diy.stackexchange.com/12345");
        assert_eq!(parse_question_item(&item), Some((diy, 12345)));
        assert_eq!(
            diy.question_url(12345),
            "https://diy.stackexchange.com/questions/12345"
        );
        assert_eq!(
            diy.dump_url(),
            "https://archive.org/download/stackexchange/diy.stackexchange.com.7z"
        );
    }

    #[test]
    fn unknown_sites_and_bad_ids_do_not_read() {
        assert_eq!(parse_question_item("evil.example/1"), None);
        assert_eq!(parse_question_item("stackoverflow.com/1"), None);
        assert_eq!(parse_question_item("diy.stackexchange.com/"), None);
        assert_eq!(parse_question_item("diy.stackexchange.com/1/x"), None);
        assert_eq!(parse_question_item("diy.stackexchange.com"), None);
    }

    #[test]
    fn sites_are_unique() {
        for (i, a) in SITES.iter().enumerate() {
            for b in &SITES[i + 1..] {
                assert_ne!(a.domain, b.domain);
                assert_ne!(a.api, b.api);
            }
        }
    }
}
