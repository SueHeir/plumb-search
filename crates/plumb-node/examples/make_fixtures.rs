//! Writes `fixtures/sample.wat`, the synthetic Common Crawl WAT file that the
//! end-to-end tests ingest. Run it again after changing the pages below:
//!
//! ```text
//! cargo run -p plumb-node --example make_fixtures
//! ```
//!
//! Everything here is made up. The brands and domains are real so that the
//! test queries read naturally, but the titles, descriptions and links are
//! invented, and the look-alike domains are fictional decoys. The file is
//! written uncompressed so that changes show up in diffs; the WAT reader
//! takes plain and gzipped files alike.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use anyhow::{Context, Result};
use plumb_ingest::{WatPage, WatWriter};

fn main() -> Result<()> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let dir = dir
        .canonicalize()
        .with_context(|| format!("finding {}", dir.display()))?;
    let path = dir.join("sample.wat");
    let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    let mut writer = WatWriter::new(BufWriter::new(file), false, "sample.wat")?;
    let pages = pages();
    for page in &pages {
        writer.write_page(page)?;
    }
    writer.finish()?;
    println!("wrote {} pages to {}", pages.len(), path.display());
    Ok(())
}

/// A page fetched with status 200.
fn page(
    url: &str,
    title: &str,
    description: Option<&str>,
    site_name: Option<&str>,
    links: &[(&str, &str)],
) -> WatPage {
    WatPage {
        url: url.to_string(),
        status: 200,
        title: Some(title.to_string()),
        description: description.map(str::to_string),
        site_name: site_name.map(str::to_string),
        links: links
            .iter()
            .map(|(href, text)| (href.to_string(), text.to_string()))
            .collect(),
    }
}

/// A response without HTML metadata, such as a redirect or a 404.
fn status_only(url: &str, status: u16) -> WatPage {
    WatPage {
        url: url.to_string(),
        status,
        ..WatPage::default()
    }
}

fn pages() -> Vec<WatPage> {
    let mut pages = Vec::new();
    pages.extend(official_homepages());
    pages.extend(lookalike_homepages());
    pages.extend(pages_linking_by_name());
    pages
}

/// Homepages of the brands in `brand_queries.tsv`, plus real sites that
/// share a name with one of them (Chase Center, Delta Faucet, Amazon Jobs,
/// the Ford Foundation). Several link to social networks, as real footers do.
fn official_homepages() -> Vec<WatPage> {
    vec![
        // Banks and payments.
        page(
            "https://www.usbank.com/",
            "Personal and Business Banking | U.S. Bank",
            Some("U.S. Bank offers checking and savings accounts, credit cards, mortgages, auto loans and wealth management."),
            Some("U.S. Bank"),
            &[
                ("/credit-cards.html", "Credit cards"),
                ("/home-loans.html", "Home loans"),
                ("https://www.facebook.com/usbank", "Facebook"),
                ("https://www.youtube.com/usbank", "YouTube"),
            ],
        ),
        page(
            "https://www.bankofamerica.com/",
            "Bank of America - Banking, Credit Cards, Loans and Merrill Investing",
            Some("What would you like the power to do? At Bank of America, our purpose is to help make financial lives better through the power of every connection."),
            Some("Bank of America"),
            &[
                ("/credit-cards/", "Credit Cards"),
                ("https://www.facebook.com/BankofAmerica", "Facebook"),
                ("https://www.youtube.com/bankofamerica", "YouTube"),
            ],
        ),
        page(
            "https://www.chase.com/",
            "Chase Bank - Credit Cards, Mortgages, Commercial Banking, Auto Loans",
            Some("With Chase online banking you can manage your accounts, view statements, pay bills and transfer funds securely from one place."),
            Some("Chase"),
            &[
                ("/personal/checking", "Checking accounts"),
                ("https://www.chasecenter.com/", "Chase Center"),
                ("https://www.facebook.com/chase", "Facebook"),
                ("https://www.youtube.com/chase", "YouTube"),
            ],
        ),
        page(
            "https://www.wellsfargo.com/",
            "Wells Fargo Bank | Financial Services & Online Banking",
            Some("Committed to the financial health of our customers and communities. Explore bank accounts, loans, mortgages, investing, credit cards and banking services."),
            Some("Wells Fargo"),
            &[
                ("/checking/", "Checking"),
                ("https://www.facebook.com/wellsfargo", "Facebook"),
            ],
        ),
        page(
            "https://www.citi.com/",
            "Citi.com: Credit Cards, Banking, Mortgage, Personal Loans",
            Some("Citibank offers checking and savings accounts, credit cards, personal loans and mortgages."),
            Some("Citi"),
            &[],
        ),
        page(
            "https://www.capitalone.com/",
            "Capital One | Credit Cards, Checking, Savings & Auto Loans",
            Some("Capital One offers credit cards, checking and savings accounts, auto loans and commercial banking services."),
            Some("Capital One"),
            &[],
        ),
        page(
            "https://www.americanexpress.com/",
            "American Express Credit Cards, Rewards & Banking",
            Some("Explore credit cards, rewards programs, travel and business services from American Express."),
            Some("American Express"),
            &[("https://www.delta.com/skymiles", "Delta SkyMiles")],
        ),
        page(
            "https://www.paypal.com/",
            "PayPal: Pay, Send and Save Money Online",
            Some("PayPal is the safer, easier way to pay and get paid online. Send money to friends, shop online and manage your account."),
            Some("PayPal"),
            &[],
        ),
        // Government. ssa.gov is seen over plain http first; the https
        // homepage that comes later should replace it.
        page(
            "https://www.irs.gov/",
            "Internal Revenue Service (IRS) | An official website of the United States government",
            Some("Find tax forms, check your refund status, make a payment and get answers to your tax questions."),
            Some("IRS"),
            &[
                ("https://www.usa.gov/", "USA.gov"),
                ("https://www.ssa.gov/", "Social Security Administration"),
            ],
        ),
        page("http://www.ssa.gov/", "SSA - Home", None, None, &[]),
        page(
            "https://www.ssa.gov/",
            "The United States Social Security Administration",
            Some("Apply for retirement, disability and survivor benefits, get a replacement Social Security card, and manage your benefits online."),
            Some("Social Security"),
            &[
                ("https://www.medicare.gov/", "Medicare.gov"),
                ("https://www.irs.gov/", "IRS"),
            ],
        ),
        page(
            "https://www.usps.com/",
            "Welcome | USPS",
            Some("Track packages, pay and print postage with Click-N-Ship, schedule free package pickups, look up ZIP Codes and find post office locations."),
            Some("USPS"),
            &[("https://www.usa.gov/", "USA.gov")],
        ),
        page(
            "https://www.medicare.gov/",
            "Medicare.gov: the official U.S. government site for Medicare",
            Some("Compare Medicare plans, find doctors and learn what Medicare covers."),
            Some("Medicare.gov"),
            &[("https://www.ssa.gov/", "Social Security")],
        ),
        page(
            "https://www.nasa.gov/",
            "NASA",
            Some("NASA.gov brings you the latest news, images and videos from America's space agency, pioneering the future in space exploration, scientific discovery and aeronautics research."),
            Some("NASA"),
            &[
                ("https://www.facebook.com/NASA", "Facebook"),
                ("https://www.youtube.com/@NASA", "YouTube"),
            ],
        ),
        page(
            "https://www.cdc.gov/",
            "Centers for Disease Control and Prevention | CDC",
            Some("CDC works 24/7 protecting America's health, safety and security."),
            Some("CDC"),
            &[("https://www.usa.gov/", "USA.gov")],
        ),
        page(
            "https://www.va.gov/",
            "VA.gov Home | Veterans Affairs",
            Some("Apply for and manage the VA benefits and services you've earned as a Veteran, Servicemember or family member, like health care, disability, education and more."),
            Some("Veterans Affairs"),
            &[],
        ),
        // Retail and restaurants.
        page(
            "https://www.amazon.com/",
            "Amazon.com. Spend less. Smile more.",
            Some("Free shipping on millions of items. Get the best of shopping and entertainment with Prime."),
            Some("Amazon"),
            &[
                ("/gp/help/customer/display.html", "Help"),
                ("https://www.amazon.jobs/", "Careers"),
            ],
        ),
        page(
            "https://www.amazon.jobs/",
            "Amazon Jobs",
            Some("Join us at Amazon and help build the future: search open jobs in software development, operations, fulfillment centers and more."),
            Some("Amazon Jobs"),
            &[("https://www.amazon.com/", "Amazon.com")],
        ),
        page(
            "https://www.walmart.com/",
            "Walmart | Save Money. Live better.",
            Some("Shop Walmart.com today for every day low prices on groceries, electronics, home goods and more."),
            Some("Walmart.com"),
            &[("https://www.facebook.com/walmart", "Facebook")],
        ),
        page(
            "https://www.target.com/",
            "Target : Expect More. Pay Less.",
            Some("Shop Target online and in-store for everything from groceries and essentials to clothing and electronics."),
            Some("Target"),
            &[("https://www.facebook.com/target", "Facebook")],
        ),
        page(
            "https://www.homedepot.com/",
            "The Home Depot",
            Some("Shop online for all your home improvement needs: appliances, tools, lumber, flooring, paint, lighting and more."),
            Some("The Home Depot"),
            &[],
        ),
        page(
            "https://www.bestbuy.com/",
            "Best Buy | Official Online Store | Shop Now & Save",
            Some("Shop Best Buy for electronics, computers, appliances, cell phones, video games and more new tech."),
            Some("Best Buy"),
            &[],
        ),
        page(
            "https://www.ikea.com/",
            "IKEA - Furniture, home furnishings and inspiration",
            Some("Affordable furniture and home furnishing ideas for every room."),
            Some("IKEA"),
            &[],
        ),
        page(
            "https://www.mcdonalds.com/",
            "McDonald's: Burgers, Fries & More. Quality Ingredients.",
            Some("Explore the McDonald's menu, find a restaurant near you and order delivery."),
            Some("McDonald's"),
            &[],
        ),
        // Airlines, and a faucet maker that shares a name with one.
        page(
            "https://www.delta.com/",
            "Delta Air Lines | Flights & Plane Tickets + Book Online",
            Some("Book a trip, check in, change seats, track your bag, check flight status, and more."),
            Some("Delta Air Lines"),
            &[("https://www.facebook.com/delta", "Facebook")],
        ),
        page(
            "https://www.deltafaucet.com/",
            "Delta Faucet | Kitchen & Bathroom Faucets & Fixtures",
            Some("Delta Faucet makes kitchen and bathroom faucets, shower heads and bath accessories."),
            Some("Delta Faucet"),
            &[],
        ),
        page(
            "https://www.united.com/",
            "United Airlines - Airline Tickets, Travel Deals and Flights",
            Some("Find low fares on United Airlines flights, check flight status and manage your MileagePlus account."),
            Some("United Airlines"),
            &[],
        ),
        page(
            "https://www.aa.com/",
            "American Airlines - Airline tickets and low fares at aa.com",
            Some("Book low fares to destinations around the world and find the latest deals on airline tickets, hotels, car rentals and vacations at aa.com."),
            Some("American Airlines"),
            &[],
        ),
        page(
            "https://www.southwest.com/",
            "Southwest Airlines | Book Flights, Airline Tickets, Airfare",
            Some("Book low fares to destinations across the U.S. and beyond on Southwest Airlines."),
            Some("Southwest Airlines"),
            &[],
        ),
        // News and sports.
        page(
            "https://www.nytimes.com/",
            "The New York Times - Breaking News, US News, World News and Videos",
            Some("Live news, investigations, opinion, photos and video by the journalists of The New York Times from more than 150 countries around the world."),
            Some("The New York Times"),
            &[
                ("https://www.facebook.com/nytimes", "Facebook"),
                ("https://www.youtube.com/nytimes", "YouTube"),
            ],
        ),
        page(
            "https://www.bbc.co.uk/",
            "BBC - Home",
            Some("The best of the BBC, with the latest news and sport headlines, weather, TV and radio highlights and much more from across the whole of BBC Online."),
            Some("BBC"),
            &[("https://www.bbc.com/news", "BBC News")],
        ),
        page(
            "https://www.bbc.com/",
            "BBC Home - Breaking News, World News, US News, Sports, Business, Innovation, Climate, Culture, Travel, Video & Audio",
            Some("Visit BBC for trusted reporting on the latest world and US news, sports, business, climate, innovation, culture and much more."),
            Some("BBC"),
            &[("https://www.bbc.co.uk/", "BBC UK")],
        ),
        page(
            "https://www.cnn.com/",
            "CNN: Breaking News, Latest News and Videos",
            Some("View the latest news and breaking news today for U.S., world, weather, entertainment, politics and health at CNN.com."),
            Some("CNN"),
            &[
                ("https://www.facebook.com/CNN", "Facebook"),
                ("https://www.youtube.com/user/CNN", "YouTube"),
            ],
        ),
        page(
            "https://www.espn.com/",
            "ESPN - Serving Sports Fans. Anytime. Anywhere.",
            Some("Visit ESPN for live scores, highlights and sports news. Stream exclusive games on ESPN+ and play fantasy sports."),
            Some("ESPN.com"),
            &[("https://www.nba.com/", "NBA")],
        ),
        // Tech.
        page(
            "https://www.google.com/",
            "Google",
            None,
            None,
            &[("https://www.youtube.com/", "YouTube")],
        ),
        page(
            "https://www.facebook.com/",
            "Facebook - log in or sign up",
            Some("Log into Facebook to start sharing and connecting with your friends, family, and people you know."),
            Some("Facebook"),
            &[],
        ),
        page(
            "https://www.youtube.com/",
            "YouTube",
            Some("Enjoy the videos and music you love, upload original content, and share it all with friends, family, and the world on YouTube."),
            Some("YouTube"),
            &[("https://www.google.com/", "Google")],
        ),
        page(
            "https://www.microsoft.com/",
            "Microsoft – AI, Cloud, Productivity, Computing, Gaming & Apps",
            Some("Explore Microsoft products and services and support for your home or business. Shop Microsoft 365, Teams, Xbox, Windows, Azure, Surface and more."),
            Some("Microsoft"),
            // github.com is in no rank list: it is only discovered through this link.
            &[("https://github.com/microsoft", "GitHub")],
        ),
        page(
            "https://www.apple.com/",
            "Apple",
            Some("Discover the innovative world of Apple and shop everything iPhone, iPad, Apple Watch, Mac, and Apple TV, plus explore accessories, entertainment, and expert device support."),
            Some("Apple"),
            &[],
        ),
        page(
            "https://www.netflix.com/",
            "Netflix - Watch TV Shows Online, Watch Movies Online",
            Some("Watch Netflix movies & TV shows online or stream right to your smart TV, game console, PC, Mac, mobile, tablet and more."),
            Some("Netflix"),
            &[],
        ),
        page(
            "https://www.wikipedia.org/",
            "Wikipedia",
            Some("Wikipedia is a free online encyclopedia, created and edited by volunteers around the world and hosted by the Wikimedia Foundation."),
            None,
            // Same registrable domain: not an inbound link.
            &[("https://en.wikipedia.org/", "English")],
        ),
        // Universities; Harvard's homepage is seen as /index.html.
        page(
            "https://www.mit.edu/",
            "MIT - Massachusetts Institute of Technology",
            Some("MIT is dedicated to advancing knowledge and educating students in science, technology and other areas of scholarship."),
            Some("MIT"),
            &[],
        ),
        page(
            "https://www.harvard.edu/index.html",
            "Harvard University",
            Some("Harvard University is devoted to excellence in teaching, learning, and research, and to developing leaders who make a difference globally."),
            Some("Harvard University"),
            &[],
        ),
        page(
            "https://www.stanford.edu/",
            "Stanford University",
            Some("Stanford University, one of the world's leading teaching and research universities, is located in the heart of Silicon Valley."),
            Some("Stanford University"),
            &[],
        ),
        // Others, including two pairs of real sites that share a name.
        page(
            "https://www.att.com/",
            "AT&T Official Site - Unlimited Data Plans, Internet Service, & TV",
            Some("Shop AT&T for the latest phones, unlimited data plans, fiber internet and TV."),
            Some("AT&T"),
            &[],
        ),
        page(
            "https://www.ford.com/",
            "Ford - New Hybrid & Electric Vehicles, SUVs, Crossovers, Trucks, Vans & Cars",
            Some("Explore the new Ford lineup of trucks, SUVs, hybrids and electric vehicles, and find a dealer near you."),
            Some("Ford"),
            &[],
        ),
        page(
            "https://www.fordfoundation.org/",
            "Home - Ford Foundation",
            Some("The Ford Foundation is an independent organization working to address inequality and build a future grounded in justice."),
            Some("Ford Foundation"),
            &[],
        ),
        page(
            "https://www.chasecenter.com/",
            "Chase Center | San Francisco's Home of the Golden State Warriors",
            Some("Chase Center is a sports and entertainment arena in San Francisco's Mission Bay, home of the Golden State Warriors."),
            Some("Chase Center"),
            &[
                ("https://www.chase.com/", "Chase"),
                ("https://www.nba.com/warriors", "Golden State Warriors"),
            ],
        ),
    ]
}

/// Fictional phishing-style look-alikes: keyword-stuffed titles, no rank
/// signals, and links only from one spam page.
fn lookalike_homepages() -> Vec<WatPage> {
    vec![
        page(
            "https://usbank-login-help.com/",
            "US Bank Login Help | US Bank Online Banking Sign In",
            Some("US Bank login help: sign in to US Bank online banking, reset your US Bank password and unlock your US Bank account."),
            Some("US Bank Login Help"),
            &[],
        ),
        page(
            "https://irs-tax-refund-help.com/",
            "IRS Tax Refund Help | Check IRS Refund Status | IRS Online",
            Some("IRS refund status, IRS tax refund help and IRS online account support. Claim your IRS tax refund today."),
            None,
            &[],
        ),
        page(
            "https://wellsfargo-secure-online.com/",
            "Wells Fargo Secure Online | Wells Fargo Sign On",
            Some("Wells Fargo secure sign on. Verify your Wells Fargo online banking account to avoid suspension."),
            None,
            &[],
        ),
        page(
            "https://paypal-account-verify.com/",
            "PayPal Account Verify | PayPal Login | PayPal Resolution Center",
            Some("Your PayPal account is limited. Verify your PayPal account information to restore PayPal access."),
            None,
            &[],
        ),
        page(
            "https://usps-package-redelivery.com/",
            "USPS Package Redelivery | USPS Tracking Update",
            Some("Your USPS package could not be delivered. Update your address to schedule USPS redelivery."),
            None,
            &[],
        ),
        page(
            "https://microsoft-support-helpline.com/",
            "Microsoft Support Helpline | Call Microsoft Tech Support | Windows Help",
            Some("Call Microsoft support now. Microsoft certified technicians fix Windows errors, Microsoft account and Microsoft Office problems."),
            None,
            &[],
        ),
        page(
            "https://facebook-login-recover.com/",
            "Facebook Login Recover | Facebook Account Recovery | Facebook Sign In",
            Some("Recover your Facebook account and Facebook password. Facebook login help."),
            None,
            &[],
        ),
        page(
            "https://amazon-prime-refund.com/",
            "Amazon Prime Refund | Amazon Account Locked | Amazon Support",
            Some("Your Amazon Prime membership refund is ready. Unlock your Amazon account and claim your Amazon refund."),
            None,
            &[],
        ),
    ]
}

/// Inner pages whose links name the sites they point to. Short names such
/// as "BofA", "Amex" and "NYT" appear only here, so those queries can only
/// be answered from link text. Generic text ("click here") and bare URLs
/// should be dropped by the WAT reader.
fn pages_linking_by_name() -> Vec<WatPage> {
    vec![
        page(
            "https://www.nerdwallet.com/best/banking/big-banks",
            "Best Big Banks of 2026 - NerdWallet",
            Some("Compare the biggest U.S. banks by fees, rates and branches."),
            Some("NerdWallet"),
            &[
                ("https://www.usbank.com/", "U.S. Bank"),
                ("https://www.bankofamerica.com/", "Bank of America"),
                ("https://www.chase.com/", "Chase"),
                ("https://www.wellsfargo.com/", "Wells Fargo"),
                ("https://www.citi.com/", "Citibank"),
                ("https://www.capitalone.com/", "Capital One"),
                ("https://www.americanexpress.com/", "American Express"),
                ("/reviews/banking", "Bank reviews"),
            ],
        ),
        page(
            "https://www.nerdwallet.com/best/credit-cards/travel",
            "Best Travel Credit Cards - NerdWallet",
            None,
            Some("NerdWallet"),
            &[
                ("https://www.americanexpress.com/us/credit-cards/", "Amex"),
                ("https://www.chase.com/sapphire", "Chase Sapphire"),
                ("https://www.delta.com/skymiles", "Delta SkyMiles"),
                ("https://www.united.com/mileageplus", "United MileagePlus"),
                (
                    "https://www.aa.com/aadvantage",
                    "American Airlines AAdvantage",
                ),
                (
                    "https://www.southwest.com/rapidrewards/",
                    "Southwest Rapid Rewards",
                ),
            ],
        ),
        page(
            "https://en.wikipedia.org/wiki/List_of_largest_banks_in_the_United_States",
            "List of largest banks in the United States - Wikipedia",
            None,
            Some("Wikipedia"),
            &[
                ("https://www.chase.com/", "JPMorgan Chase"),
                ("https://www.bankofamerica.com/", "Bank of America"),
                ("https://www.wellsfargo.com/", "Wells Fargo"),
                ("https://www.citi.com/", "Citigroup"),
                ("https://www.usbank.com/", "U.S. Bancorp"),
                ("https://www.capitalone.com/", "Capital One"),
                ("/wiki/Bank", "Bank"),
            ],
        ),
        page(
            "https://en.wikipedia.org/wiki/List_of_research_universities_in_the_United_States",
            "List of research universities in the United States - Wikipedia",
            None,
            Some("Wikipedia"),
            &[
                (
                    "https://web.mit.edu/",
                    "Massachusetts Institute of Technology",
                ),
                ("https://www.harvard.edu/", "Harvard University"),
                ("https://www.stanford.edu/", "Stanford University"),
            ],
        ),
        page(
            "https://www.reddit.com/r/personalfinance/comments/1abc2de/which_big_bank_do_you_use/",
            "Which big bank do you use? : r/personalfinance",
            None,
            Some("Reddit"),
            &[
                ("https://www.bankofamerica.com/", "BofA"),
                ("https://www.americanexpress.com/", "Amex"),
                ("https://www.chase.com/", "Chase"),
                ("https://www.usbank.com/", "us bank"),
                ("https://www.usbank.com/", "click here"),
                ("https://www.wellsfargo.com/", "https://www.wellsfargo.com/"),
                (
                    "https://www.nytimes.com/2026/09/01/business/bank-fees.html",
                    "NYT",
                ),
                ("/r/personalfinance/wiki", "wiki"),
            ],
        ),
        page(
            "https://www.reddit.com/r/cscareerquestions/comments/1xyz9ab/amazon_interview_loop/",
            "Amazon interview loop : r/cscareerquestions",
            None,
            Some("Reddit"),
            &[
                ("https://www.amazon.jobs/en/", "Amazon Jobs"),
                ("https://www.amazon.com/", "Amazon"),
            ],
        ),
        page(
            "https://www.usa.gov/agency-index",
            "A-Z index of U.S. government departments and agencies | USAGov",
            None,
            Some("USAGov"),
            &[
                ("https://www.irs.gov/", "Internal Revenue Service (IRS)"),
                (
                    "https://www.ssa.gov/",
                    "Social Security Administration (SSA)",
                ),
                ("https://www.usps.com/", "U.S. Postal Service (USPS)"),
                ("https://www.medicare.gov/", "Medicare"),
                (
                    "https://www.nasa.gov/",
                    "National Aeronautics and Space Administration (NASA)",
                ),
                (
                    "https://www.cdc.gov/",
                    "Centers for Disease Control and Prevention (CDC)",
                ),
                ("https://www.va.gov/", "Department of Veterans Affairs (VA)"),
            ],
        ),
        page(
            "https://www.cnn.com/2026/09/14/business/bank-earnings-week",
            "Big bank earnings: what to watch | CNN Business",
            None,
            Some("CNN"),
            &[
                ("https://www.bankofamerica.com/", "BofA"),
                ("https://www.americanexpress.com/", "Amex"),
                ("https://www.wellsfargo.com/", "Wells Fargo"),
                ("https://www.nytimes.com/", "NYT"),
                ("https://www.irs.gov/", "IRS"),
            ],
        ),
        page(
            "https://www.bbc.co.uk/news/world-us-canada-70000001",
            "US space agency delays launch - BBC News",
            None,
            Some("BBC News"),
            &[
                ("https://www.nasa.gov/", "Nasa"),
                ("https://www.nytimes.com/", "New York Times"),
                ("https://www.cdc.gov/", "CDC"),
                ("https://www.ssa.gov/", "Social Security"),
            ],
        ),
        page(
            "https://www.bbc.com/news/business-70000002",
            "Carmakers and airlines report earnings - BBC News",
            None,
            Some("BBC News"),
            &[
                ("https://www.ford.com/", "Ford"),
                ("https://www.fordfoundation.org/", "Ford Foundation"),
                ("https://www.delta.com/", "Delta Air Lines"),
                ("https://www.united.com/", "United Airlines"),
                ("https://www.aa.com/", "American Airlines"),
                ("https://www.southwest.com/", "Southwest Airlines"),
            ],
        ),
        page(
            "https://slickdeals.net/deals/home/",
            "Home & Home Improvement Deals | Slickdeals",
            None,
            Some("Slickdeals"),
            &[
                ("https://www.walmart.com/", "Walmart"),
                ("https://www.target.com/", "Target"),
                ("https://www.homedepot.com/", "The Home Depot"),
                ("https://www.bestbuy.com/", "Best Buy"),
                ("https://www.ikea.com/", "IKEA"),
                ("https://www.amazon.com/", "Amazon"),
                ("https://www.mcdonalds.com/", "McDonald's"),
                ("https://www.att.com/", "AT&T"),
                ("https://www.netflix.com/", "Netflix"),
                ("https://www.apple.com/", "Apple"),
                ("https://www.microsoft.com/", "Microsoft"),
                ("https://www.paypal.com/", "PayPal"),
            ],
        ),
        page(
            "https://www.espn.com/nba/team/_/name/gs/golden-state-warriors",
            "Golden State Warriors Scores, Stats and Highlights - ESPN",
            None,
            Some("ESPN"),
            &[
                ("https://www.chasecenter.com/", "Chase Center"),
                ("https://www.nba.com/warriors", "Warriors"),
            ],
        ),
        // A spam page, the only thing linking to the look-alikes.
        page(
            "https://free-gift-cards-online.net/top-sites.html",
            "Top Sites 2026 - Free Gift Cards Online",
            None,
            None,
            &[
                ("https://usbank-login-help.com/", "US Bank Login"),
                ("https://irs-tax-refund-help.com/", "IRS Refund"),
                ("https://paypal-account-verify.com/", "PayPal Login"),
            ],
        ),
        // Responses without HTML metadata.
        status_only("http://usbank.com/", 301),
        status_only("https://www.nerdwallet.com/old-page", 404),
    ]
}
