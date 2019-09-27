use bitflags::bitflags;
use bytes::BytesMut;
use futures::task::Task;

use tokio::codec::{Decoder, Encoder};

use crate::com::AsError;
use crate::protocol::IntoReply;
use crate::proxy::standalone::Request;
use crate::utils::notify::Notify;
use crate::utils::trim_hash_tag;

use std::cell::RefCell;
use std::rc::Rc;

pub mod msg;
use self::msg::Message;

const MAX_CYCLE: u8 = 8;

#[derive(Clone, Debug)]
pub struct Cmd {
    cmd: Rc<RefCell<Command>>,
    notify: Notify,
}

impl Drop for Cmd {
    fn drop(&mut self) {
        let expect = self.notify.expect();
        let origin = self.notify.fetch_sub(1);
        // TODO: sub command maybe notify multiple
        trace!("cmd drop strong ref {} and expect {}", origin, expect);
        if origin - 1 == expect {
            self.notify.notify();
        }
    }
}

impl Request for Cmd {
    type Reply = Message;
    type FrontCodec = FrontCodec;
    type BackCodec = BackCodec;

    fn ping_request() -> Self {
        let cmd = Command {
            ctype: CmdType::Read,
            flags: Flags::empty(),
            cycle: 0,

            req: Message::version_request(),
            reply: None,
            subs: None,
        };
        Cmd {
            cmd: Rc::new(RefCell::new(cmd)),
            notify: Notify::empty(),
        }
    }

    fn reregister(&mut self, task: Task) {
        self.notify.set_task(task);
    }

    fn key_hash(&self, hash_tag: &[u8], hasher: fn(&[u8]) -> u64) -> u64 {
        let cmd = self.cmd.borrow();
        let key = cmd.req.get_key();
        hasher(trim_hash_tag(key, hash_tag))
    }

    fn subs(&self) -> Option<Vec<Self>> {
        self.cmd.borrow().subs.clone()
    }

    fn is_done(&self) -> bool {
        if let Some(subs) = self.subs() {
            subs.iter().all(|x| x.is_done())
        } else {
            self.cmd.borrow().is_done()
        }
    }

    fn add_cycle(&self) {
        self.cmd.borrow_mut().add_cycle()
    }
    fn can_cycle(&self) -> bool {
        self.cmd.borrow().can_cycle()
    }

    fn is_error(&self) -> bool {
        self.cmd.borrow().is_error()
    }

    fn valid(&self) -> bool {
        true
    }

    fn set_reply<R: IntoReply<Message>>(&self, t: R) {
        let reply = t.into_reply();
        self.cmd.borrow_mut().set_reply(reply);
        self.cmd.borrow_mut().set_done();
    }

    fn set_error(&self, t: &AsError) {
        let reply: Message = t.into_reply();
        self.cmd.borrow_mut().set_reply(reply);
        self.cmd.borrow_mut().set_done();
        self.cmd.borrow_mut().set_error();
    }
}

impl Cmd {
    fn from_msg(msg: Message, mut notify: Notify) -> Cmd {
        let flags = Flags::empty();
        let ctype = CmdType::Read;
        let sub_msgs = msg.mk_subs();
        notify.set_expect((1 + sub_msgs.len()) as u16);

        let subs: Vec<_> = sub_msgs
            .into_iter()
            .map(|sub_msg| {
                let command = Command {
                    ctype: ctype.clone(),
                    flags: flags.clone(),
                    cycle: 0,
                    req: sub_msg,
                    reply: None,
                    subs: None,
                };
                Cmd {
                    notify: notify.clone(),
                    cmd: Rc::new(RefCell::new(command)),
                }
            })
            .collect();
        let subs = if subs.is_empty() { None } else { Some(subs) };
        let command = Command {
            ctype: CmdType::Read,
            flags: Flags::empty(),
            cycle: 0,
            req: msg,
            reply: None,
            subs,
        };
        Cmd {
            cmd: Rc::new(RefCell::new(command)),
            notify,
        }
    }
}

impl From<Message> for Cmd {
    fn from(msg: Message) -> Cmd {
        Cmd::from_msg(msg, Notify::empty())
    }
}

bitflags! {
    pub struct Flags: u8 {
        const DONE  = 0b00000001;
        const ERROR = 0b00000010;
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmdType {
    Read,
    Write,
    Ctrl,
    NotSupport,
}

#[derive(Clone, Debug)]
pub struct Command {
    ctype: CmdType,
    flags: Flags,
    cycle: u8,

    req: Message,
    reply: Option<Message>,

    subs: Option<Vec<Cmd>>,
}

impl Command {
    fn is_done(&self) -> bool {
        self.flags & Flags::DONE == Flags::DONE
    }

    fn is_error(&self) -> bool {
        self.flags & Flags::ERROR == Flags::ERROR
    }

    pub fn can_cycle(&self) -> bool {
        self.cycle < MAX_CYCLE
    }

    pub fn add_cycle(&mut self) {
        self.cycle += 1;
    }

    pub fn set_reply(&mut self, reply: Message) {
        self.reply = Some(reply);
    }

    pub fn set_done(&mut self) {
        self.flags |= Flags::DONE;
    }

    pub fn set_error(&mut self) {
        self.flags |= Flags::ERROR;
    }
}

#[derive(Default)]
pub struct FrontCodec {}

impl Decoder for FrontCodec {
    type Item = Cmd;
    type Error = AsError;
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        match Message::parse(src).map(|x| x.map(Into::into)) {
            Ok(val) => Ok(val),
            Err(AsError::BadMessage) => {
                let cmd: Cmd = Message::raw_inline_reply().into();
                cmd.set_error(&AsError::BadMessage);
                Ok(Some(cmd))
            }
            Err(err) => Err(err),
        }
    }
}

impl Encoder for FrontCodec {
    type Item = Cmd;
    type Error = AsError;
    fn encode(&mut self, item: Self::Item, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let mut cmd = item.cmd.borrow_mut();
        if let Some(subs) = cmd.subs.as_ref().cloned() {
            for sub in subs {
                self.encode(sub, dst)?;
            }
            cmd.req.try_save_ends(dst);
        } else {
            let reply = cmd.reply.take().expect("reply must exits");
            cmd.req.save_reply(reply, dst)?;
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct BackCodec {}

impl Decoder for BackCodec {
    type Item = Message;
    type Error = AsError;
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        Message::parse(src)
    }
}

impl Encoder for BackCodec {
    type Item = Cmd;
    type Error = AsError;
    fn encode(&mut self, item: Self::Item, dst: &mut BytesMut) -> Result<(), Self::Error> {
        item.cmd.borrow().req.save_req(dst)
    }
}
