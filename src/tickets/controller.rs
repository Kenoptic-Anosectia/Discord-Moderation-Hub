use super::{
    policy::{marker, Kind, Settings},
    repository::{Panel, Repository, Ticket},
};
use crate::Error;
#[derive(Clone, Debug)]
pub struct Request {
    pub interaction: u64,
    pub guild: Option<u64>,
    pub channel: u64,
    pub message: u64,
    pub author: u64,
    pub author_is_bot: bool,
    pub bot: u64,
    pub opener: u64,
    pub custom_id: String,
}
#[derive(Clone, Debug)]
pub struct RemoteChannel {
    pub id: u64,
    pub guild: u64,
    pub category: u64,
    pub topic: String,
    pub private: bool,
}
#[derive(Debug, Clone, Copy)]
pub enum CreateFailure {
    NotCreated,
    Unknown,
}
#[serenity::async_trait]
pub trait Discord: Send + Sync {
    async fn defer_private(&self) -> Result<(), Error>;
    async fn feedback_private(&self, text: &str) -> Result<(), Error>;
    async fn validate(&self, kind: Kind) -> Result<(), Error>;
    async fn create(&self, ticket: &Ticket) -> Result<u64, CreateFailure>;
    async fn channels(&self, _ticket: &Ticket) -> Result<Vec<RemoteChannel>, Error>;
    async fn greet(&self, channel: u64, kind: Kind) -> Result<(), Error>;
}
pub async fn route<D: Discord>(
    settings: &Settings,
    repo: &Repository,
    request: &Request,
    io: &D,
    opening_approved: bool,
) -> Result<(), Error> {
    if !settings.valid()
        || request.guild != Some(settings.guild)
        || request.opener == 0
        || request.bot == 0
        || !request.author_is_bot
        || request.author != request.bot
    {
        return Ok(());
    }
    let Some(kind) = Kind::parse(&request.custom_id) else {
        return Ok(());
    };
    // Defer locally plausible requests before even the provenance lookup. A copied panel
    // receives only private rejection; it never reaches validation/reservation/create.
    io.defer_private().await?;
    let result:Result<String,Error>=async {
  let panel=Panel {guild:settings.guild,channel:request.channel,message:request.message,bot:request.bot};
  if !repo.trusted_panel(panel).await? { return Ok("This panel is not recognized. Ask the server owner for the current panel.".to_string()); }
  if !opening_approved { return Ok("Ticket opening is disabled pending privacy and archive rollout review.".to_string()); }
  io.validate(kind).await?;
  let ticket=repo.reserve(request.interaction,settings.guild,request.opener,kind).await?;
  if ticket.guild!=settings.guild || ticket.opener!=request.opener || ticket.kind!=kind { return Err("interaction binding mismatch".into()); }
  if let Some(channel)=ticket.channel {return Ok(format!("Your existing ticket is <#{channel}>. Ownership remains reserved."));}
  if ticket.status=="failed" {return Ok("This operation did not create a channel. Use a new request to try again.".into());}
  if !repo.claim(ticket.id).await? {
   let matches:Vec<_>=io.channels(&ticket).await?.into_iter().filter(|c|c.guild==settings.guild && c.category==settings.category(kind) && c.topic==marker(&ticket.operation) && c.private).collect();
   // Metadata can be forged by users with channel management privileges. It is
   // corroboration only; an unknown HTTP outcome is never auto-adopted.
   let ids:Vec<String>=matches.iter().map(|c|c.id.to_string()).collect();
   return Ok(format!("Operation {} needs audited operator recovery ({} unverified metadata candidates: [{}]). The slot remains reserved; no new channel was created by this retry.",ticket.id,matches.len(),ids.join(", ")));
  }
  match io.create(&ticket).await {
   Ok(channel)=> {
    // Attach before greeting: a failed greeting is not a failed channel creation.
    repo.attach(ticket.id,channel).await?;
    let greeted=io.greet(channel,kind).await.is_ok();
    repo.greeting_result(ticket.id,greeted).await?;
    Ok(format!("Your ticket is <#{channel}>.{}",if greeted {""} else {" The greeting could not be delivered; the ticket remains open."}))
   },
   Err(CreateFailure::NotCreated)=>{repo.failed(ticket.id).await?;Ok("Discord rejected channel creation; no channel was created. Use a new request to retry.".into())},
   Err(CreateFailure::Unknown)=>Ok("Channel creation could not be confirmed. The slot remains reserved for recovery; do not keep opening tickets.".into()),
  }
 }.await;
    let text=match result {Ok(text)=>text,Err(_)=>"The operation could not be confirmed. A ticket or reservation may already exist; staff may need to investigate. No automatic cleanup was performed.".to_string()};
    io.feedback_private(&text).await
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    struct Fake {
        events: Mutex<Vec<String>>,
        outcome: Option<CreateFailure>,
        remote: Vec<RemoteChannel>,
        fail_ack: bool,
        fail_validate: bool,
        fail_greet: bool,
        fail_feedback: bool,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                events: Mutex::new(vec![]),
                outcome: None,
                remote: vec![],
                fail_ack: false,
                fail_validate: false,
                fail_greet: false,
                fail_feedback: false,
            }
        }
        fn event(&self, s: &str) {
            self.events.lock().unwrap().push(s.into());
        }
    }
    #[serenity::async_trait]
    impl Discord for Fake {
        async fn defer_private(&self) -> Result<(), Error> {
            self.event("defer:private");
            if self.fail_ack {
                Err("ack failed".into())
            } else {
                Ok(())
            }
        }
        async fn feedback_private(&self, t: &str) -> Result<(), Error> {
            self.event(&format!("private:{t}"));
            if self.fail_feedback {
                Err("feedback failed".into())
            } else {
                Ok(())
            }
        }
        async fn validate(&self, _: Kind) -> Result<(), Error> {
            self.event("validate");
            tokio::task::yield_now().await;
            if self.fail_validate {
                Err("private raw details".into())
            } else {
                Ok(())
            }
        }
        async fn create(&self, t: &Ticket) -> Result<u64, CreateFailure> {
            self.event("create");
            if let Some(e) = self.outcome {
                Err(e)
            } else {
                Ok(998 + t.id as u64)
            }
        }
        async fn channels(&self, _ticket: &Ticket) -> Result<Vec<RemoteChannel>, Error> {
            self.event("channels");
            Ok(self.remote.clone())
        }
        async fn greet(&self, _: u64, _: Kind) -> Result<(), Error> {
            self.event("greet");
            if self.fail_greet {
                Err("greet failed".into())
            } else {
                Ok(())
            }
        }
    }
    fn settings() -> Settings {
        Settings {
            guild: 100,
            staff: 200,
            support_category: 300,
            age_category: 400,
            db_path: "test".into(),
        }
    }
    fn request() -> Request {
        Request {
            interaction: 1,
            guild: Some(100),
            channel: 500,
            message: 600,
            author: 700,
            author_is_bot: true,
            bot: 700,
            opener: 800,
            custom_id: super::super::policy::SUPPORT.into(),
        }
    }
    async fn repository() -> Repository {
        let r = Repository::open(":memory:").await.unwrap();
        r.save_panel(Panel {
            guild: 100,
            channel: 500,
            message: 600,
            bot: 700,
        })
        .await
        .unwrap();
        r
    }
    #[tokio::test]
    async fn uncertain_operation_never_adopts_a_single_forgeable_match() {
        for matches in [0, 1, 2] {
            let r = repository().await;
            let q = request();
            let mut io = Fake::new();
            io.outcome = Some(CreateFailure::Unknown);
            route(&settings(), &r, &q, &io, true).await.unwrap();
            let t = r.reserve(1, 100, 800, Kind::Support).await.unwrap();
            assert_eq!(t.status, "creating");
            io.remote = (0..matches)
                .map(|i| RemoteChannel {
                    id: 900 + i,
                    guild: 100,
                    category: 300,
                    topic: marker(&t.operation),
                    private: true,
                })
                .collect();
            // Decoys are never counted, even with the correct marker.
            io.remote.extend([
                RemoteChannel {
                    id: 991,
                    guild: 101,
                    category: 300,
                    topic: marker(&t.operation),
                    private: true,
                },
                RemoteChannel {
                    id: 992,
                    guild: 100,
                    category: 400,
                    topic: marker(&t.operation),
                    private: true,
                },
                RemoteChannel {
                    id: 993,
                    guild: 100,
                    category: 300,
                    topic: marker(&t.operation),
                    private: false,
                },
            ]);
            route(&settings(), &r, &q, &io, true).await.unwrap();
            let t = r.reserve(1, 100, 800, Kind::Support).await.unwrap();
            assert_eq!(
                t.channel, None,
                "even a single perfect metadata match may be forged"
            );
            assert_eq!(t.status, "creating");
            assert_eq!(
                io.events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|s| *s == "create")
                    .count(),
                1
            );
        }
    }
    #[tokio::test]
    async fn known_rejection_ack_validation_greeting_and_feedback_failures_are_truthful() {
        for mode in ["ack", "validate", "reject", "greet", "feedback"] {
            let r = repository().await;
            let mut io = Fake::new();
            match mode {
                "ack" => io.fail_ack = true,
                "validate" => io.fail_validate = true,
                "reject" => io.outcome = Some(CreateFailure::NotCreated),
                "greet" => io.fail_greet = true,
                _ => io.fail_feedback = true,
            };
            let result = route(&settings(), &r, &request(), &io, true).await;
            if ["ack", "feedback"].contains(&mode) {
                assert!(result.is_err());
            } else {
                result.unwrap();
            }
            let e = io.events.lock().unwrap().clone();
            assert!(!e.iter().any(|s| s.contains("private raw details")));
            if ["ack", "validate"].contains(&mode) {
                assert!(!e.iter().any(|s| s == "create"));
            } else {
                let t = r.reserve(1, 100, 800, Kind::Support).await.unwrap();
                if mode == "reject" {
                    assert_eq!(t.status, "failed");
                    assert!(e.iter().any(|s| s.contains("no channel was created")));
                } else {
                    assert_eq!(t.channel, Some(999));
                    assert_eq!(t.status, "active");
                    if mode == "greet" {
                        assert_eq!(t.greeting, "failed");
                        assert!(e.iter().any(|s| s.contains("remains open")));
                    }
                }
            }
        }
    }
    #[tokio::test]
    async fn concurrent_same_slot_creates_once_but_other_slots_are_independent() {
        let r = repository().await;
        let io = Fake::new();
        let q = request();
        let q2 = Request {
            interaction: 2,
            ..q.clone()
        };
        let s = settings();
        let (a, b) = tokio::join!(route(&s, &r, &q, &io, true), route(&s, &r, &q2, &io, true));
        a.unwrap();
        b.unwrap();
        assert_eq!(
            io.events
                .lock()
                .unwrap()
                .iter()
                .filter(|s| *s == "create")
                .count(),
            1
        );
        for q in [
            Request {
                interaction: 3,
                opener: 801,
                ..q.clone()
            },
            Request {
                interaction: 4,
                custom_id: super::super::policy::AGE.into(),
                ..q
            },
        ] {
            route(&settings(), &r, &q, &io, true).await.unwrap();
        }
        assert_eq!(
            io.events
                .lock()
                .unwrap()
                .iter()
                .filter(|s| *s == "create")
                .count(),
            3
        );
    }
    #[tokio::test]
    async fn trusted_route_defers_before_work_and_retries_without_creation() {
        for custom in [super::super::policy::SUPPORT, super::super::policy::AGE] {
            let r = repository().await;
            let io = Fake::new();
            let q = Request {
                custom_id: custom.into(),
                ..request()
            };
            route(&settings(), &r, &q, &io, true).await.unwrap();
            let e = io.events.lock().unwrap().clone();
            assert_eq!(e[0], "defer:private");
            assert_eq!(e[1], "validate");
            assert!(e.iter().any(|s| s == "create"));
            assert!(e.iter().any(|s| s == "greet"));
            route(&settings(), &r, &q, &io, true).await.unwrap();
            assert_eq!(
                io.events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|s| *s == "create")
                    .count(),
                1
            );
            let t = r
                .reserve(q.interaction, 100, 800, Kind::parse(custom).unwrap())
                .await
                .unwrap();
            assert_eq!(t.channel, Some(999));
            assert_eq!(t.status, "active");
        }
    }
    #[tokio::test]
    async fn untrusted_components_invalid_config_and_rollout_gate_never_create() {
        let r = repository().await;
        let base = request();
        let bad = [
            Request {
                custom_id: "unknown".into(),
                ..base.clone()
            },
            Request {
                guild: None,
                ..base.clone()
            },
            Request {
                guild: Some(101),
                ..base.clone()
            },
            Request {
                message: 601,
                ..base.clone()
            },
            Request {
                channel: 501,
                ..base.clone()
            },
            Request {
                author: 701,
                ..base.clone()
            },
            Request {
                author_is_bot: false,
                ..base.clone()
            },
        ];
        for q in bad {
            let io = Fake::new();
            route(&settings(), &r, &q, &io, true).await.unwrap();
            assert!(!io.events.lock().unwrap().iter().any(|s| s == "create"));
        }
        let io = Fake::new();
        let mut s = settings();
        s.staff = 100;
        route(&s, &r, &base, &io, true).await.unwrap();
        assert!(io.events.lock().unwrap().is_empty());
        let io = Fake::new();
        route(&settings(), &r, &base, &io, false).await.unwrap();
        let e = io.events.lock().unwrap();
        assert!(e.iter().any(|s| s.contains("disabled")));
        assert!(!e.iter().any(|s| s == "validate" || s == "create"));
    }
}
