//! Tiny User-Agent classifier: device, OS, browser and bot detection.

#[derive(Debug, PartialEq, Eq)]
pub struct UaInfo {
    pub device: &'static str,
    pub os: &'static str,
    pub browser: &'static str,
    pub bot: bool,
}

const BOT_MARKERS: &[&str] = &[
    "bot",
    "crawl",
    "spider",
    "slurp",
    "facebookexternalhit",
    "facebookcatalog",
    "embedly",
    "preview",
    "whatsapp",
    "telegram",
    "discord",
    "slack",
    "skype",
    "linkedin",
    "pinterest",
    "vkshare",
    "w3c_validator",
    "curl/",
    "wget/",
    "python",
    "go-http-client",
    "java/",
    "okhttp",
    "axios",
    "node-fetch",
    "undici",
    "headless",
    "lighthouse",
    "httpclient",
    "libwww",
    "scrapy",
    "feedfetcher",
    "monitor",
    "uptime",
    "pingdom",
    "http.rb",
    "ruby",
    "php/",
    "postman",
];

/// Crawlers that build link previews (cards) in chat apps and social networks.
/// iMessage sends a UA containing both "facebookexternalhit" and "Twitterbot".
const PREVIEW_MARKERS: &[&str] = &[
    "facebookexternalhit",
    "facebot",
    "twitterbot",
    "slackbot",
    "linkedinbot",
    "discordbot",
    "telegrambot",
    "whatsapp",
    "skypeuripreview",
    "pinterest",
    "redditbot",
    "embedly",
    "mastodon",
    "bluesky",
    "cardyb",
    "vkshare",
    "iframely",
    "google-pagerenderer",
    "applebot",
];

pub fn is_preview_crawler(ua: &str) -> bool {
    let l = ua.to_ascii_lowercase();
    PREVIEW_MARKERS.iter().any(|m| l.contains(m))
}

pub fn parse(ua: &str) -> UaInfo {
    let l = ua.to_ascii_lowercase();
    let bot = l.is_empty() || BOT_MARKERS.iter().any(|m| l.contains(m));

    let os = if l.contains("iphone") || l.contains("ipad") || l.contains("ipod") {
        "iOS"
    } else if l.contains("android") {
        "Android"
    } else if l.contains("windows") {
        "Windows"
    } else if l.contains("cros") {
        "ChromeOS"
    } else if l.contains("mac os x") || l.contains("macintosh") {
        "macOS"
    } else if l.contains("linux") {
        "Linux"
    } else {
        "Other"
    };

    let browser = if l.contains("edg/") || l.contains("edga/") || l.contains("edgios/") {
        "Edge"
    } else if l.contains("opr/") || l.contains("opera") {
        "Opera"
    } else if l.contains("samsungbrowser") {
        "Samsung Internet"
    } else if l.contains("firefox/") || l.contains("fxios/") {
        "Firefox"
    } else if l.contains("chrome/") || l.contains("crios/") {
        "Chrome"
    } else if l.contains("safari/") {
        "Safari"
    } else {
        "Other"
    };

    let device = if bot {
        "bot"
    } else if l.contains("ipad")
        || l.contains("tablet")
        || (os == "Android" && !l.contains("mobile"))
    {
        "tablet"
    } else if l.contains("mobi") || l.contains("iphone") || l.contains("ipod") {
        "mobile"
    } else {
        "desktop"
    };

    UaInfo {
        device,
        os,
        browser,
        bot,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies() {
        let iphone = parse("Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1");
        assert_eq!(
            iphone,
            UaInfo {
                device: "mobile",
                os: "iOS",
                browser: "Safari",
                bot: false
            }
        );

        let android = parse("Mozilla/5.0 (Linux; Android 15; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Mobile Safari/537.36");
        assert_eq!(
            android,
            UaInfo {
                device: "mobile",
                os: "Android",
                browser: "Chrome",
                bot: false
            }
        );

        let edge = parse("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36 Edg/130.0");
        assert_eq!(
            edge,
            UaInfo {
                device: "desktop",
                os: "Windows",
                browser: "Edge",
                bot: false
            }
        );

        let ff = parse(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 14.5; rv:132.0) Gecko/20100101 Firefox/132.0",
        );
        assert_eq!(
            (ff.os, ff.browser, ff.device),
            ("macOS", "Firefox", "desktop")
        );

        assert!(parse("Slackbot-LinkExpanding 1.0 (+https://api.slack.com/robots)").bot);
        assert!(parse("curl/8.5.0").bot);
        assert!(parse("Googlebot/2.1").bot);
        assert!(parse("").bot);
        assert!(is_preview_crawler(
            "Slackbot-LinkExpanding 1.0 (+https://api.slack.com/robots)"
        ));
        assert!(is_preview_crawler(
            "Mozilla/5.0 (compatible; Twitterbot/1.0)"
        ));
        assert!(!is_preview_crawler(
            "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) Safari/604.1"
        ));
    }
}
