use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::io;

use att::packet as pkt;
use att::server::{
    Connection as AttConnection, ErrorResponse, Handler, Outbound, RunError as AttRunError,
    Server as AttServer,
};
use att::Handle;
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::database::Database;
use crate::Registration;

#[derive(Debug)]
struct GattHandler<T> {
    db: Database,
    write_tokens: HashMap<Handle, T>,
    events_tx: mpsc::UnboundedSender<Event<T>>,
}

impl<T> GattHandler<T> {
    fn new(
        db: Database,
        write_tokens: HashMap<Handle, T>,
        events_tx: mpsc::UnboundedSender<Event<T>>,
    ) -> Self {
        Self {
            db,
            write_tokens,
            events_tx,
        }
    }
}

impl<T> Handler for GattHandler<T>
where
    T: Clone,
{
    fn handle_exchange_mtu_request(
        &mut self,
        item: &pkt::ExchangeMtuRequest,
    ) -> Result<pkt::ExchangeMtuResponse, ErrorResponse> {
        Ok(pkt::ExchangeMtuResponse::new(*item.client_rx_mtu()))
    }

    fn handle_find_information_request(
        &mut self,
        item: &pkt::FindInformationRequest,
    ) -> Result<pkt::FindInformationResponse, ErrorResponse> {
        let r = match self
            .db
            .find_information(item.starting_handle().clone()..=item.ending_handle().clone())
        {
            Ok(v) => v,
            Err((h, e)) => return Err(ErrorResponse::new(h, e)),
        };
        Ok(r.into_iter().map(Into::into).collect())
    }

    fn handle_read_by_type_request(
        &mut self,
        item: &pkt::ReadByTypeRequest,
    ) -> Result<pkt::ReadByTypeResponse, ErrorResponse> {
        let r = match self.db.read_by_type(
            item.starting_handle().clone()..=item.ending_handle().clone(),
            item.attribute_type(),
            false,
            false,
        ) {
            Ok(v) => v,
            Err((h, e)) => return Err(ErrorResponse::new(h, e)),
        };
        Ok(r.into_iter().map(Into::into).collect())
    }

    fn handle_read_request(
        &mut self,
        item: &pkt::ReadRequest,
    ) -> Result<pkt::ReadResponse, ErrorResponse> {
        let r = match self.db.read(item.attribute_handle(), false, false) {
            Ok(v) => v,
            Err((h, e)) => return Err(ErrorResponse::new(h, e)),
        };
        Ok(pkt::ReadResponse::new(r))
    }

    fn handle_read_by_group_type_request(
        &mut self,
        item: &pkt::ReadByGroupTypeRequest,
    ) -> Result<pkt::ReadByGroupTypeResponse, ErrorResponse> {
        let r = match self.db.read_by_group_type(
            item.starting_handle().clone()..=item.ending_handle().clone(),
            item.attribute_group_type(),
            false,
            false,
        ) {
            Ok(v) => v,
            Err((h, e)) => return Err(ErrorResponse::new(h, e)),
        };
        Ok(r.into_iter().map(Into::into).collect())
    }

    fn handle_write_request(
        &mut self,
        item: &pkt::WriteRequest,
    ) -> Result<pkt::WriteResponse, ErrorResponse> {
        let value = item.attribute_value();
        if let Some(token) = self.write_tokens.get(item.attribute_handle()) {
            self.events_tx
                .send(Event::Write(token.clone(), value.to_vec().into()))
                .ok();
        }

        match self.db.write(item.attribute_handle(), value, false, false) {
            Ok(_) => Ok(pkt::WriteResponse::new()),
            Err((h, e)) => Err(ErrorResponse::new(h, e)),
        }
    }

    fn handle_write_command(&mut self, item: &pkt::WriteCommand) {
        let value = item.attribute_value();
        if let Some(token) = self.write_tokens.get(item.attribute_handle()) {
            self.events_tx
                .send(Event::Write(token.clone(), value.to_vec().into()))
                .ok();
        }

        if let Err(err) = self.db.write(
            item.attribute_handle(),
            item.attribute_value(),
            false,
            false,
        ) {
            log::warn!("{:?}", err);
        };
    }

    fn handle_signed_write_command(&mut self, item: &pkt::SignedWriteCommand) {
        let value = item.attribute_value();
        if let Some(token) = self.write_tokens.get(item.attribute_handle()) {
            self.events_tx
                .send(Event::Write(token.clone(), value.to_vec().into()))
                .ok();
        }

        if let Err(err) =
            self.db
                .write(item.attribute_handle(), item.attribute_value(), false, true)
        {
            log::warn!("{:?}", err);
        };
    }
}

#[derive(Debug, thiserror::Error)]
#[error("channel error")]
pub struct OutgoingError;

#[derive(Debug)]
pub struct Outgoing<T> {
    inner: Outbound,
    token_map: HashMap<T, Handle>,
}

impl<T> Outgoing<T>
where
    T: Eq + Hash,
{
    pub fn notify<B>(&self, token: &T, val: B) -> Result<(), OutgoingError>
    where
        B: Into<Bytes>,
    {
        let handle = self.token_map.get(token).unwrap();
        self.inner
            .notify(handle.clone(), val.into())
            .map_err(|_| OutgoingError)?;
        Ok(())
    }

    pub async fn indicate<B>(&self, token: &T, val: B) -> Result<(), OutgoingError>
    where
        B: Into<Bytes>,
    {
        let handle = self.token_map.get(token).unwrap();
        self.inner
            .indicate(handle.clone(), val.into())
            .await
            .map_err(|_| OutgoingError)?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum Event<T> {
    Write(T, Bytes),
}

#[derive(Debug)]
pub struct Events<T>(mpsc::UnboundedReceiver<Event<T>>);

impl<T> Events<T> {
    pub async fn next(&mut self) -> Option<Event<T>> {
        self.0.recv().await
    }
}

#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct RunError(#[from] AttRunError);

#[derive(Debug)]
pub struct Connection {
    inner: AttConnection,
}

impl Connection {
    pub fn run<T>(
        self,
        registration: Registration<T>,
    ) -> (
        impl Future<Output = Result<(), RunError>>,
        Outgoing<T>,
        Events<T>,
    )
    where
        T: Hash + Eq + Clone,
    {
        let (db, write_tokens, notify_or_indicate_handles) = registration.build();
        let outgoing = self.inner.outbound();

        let (tx, rx) = mpsc::unbounded_channel();
        let events = Events(rx);

        let task = self.inner.run(GattHandler::<T>::new(db, write_tokens, tx));
        let task = async move {
            if let Err(e) = task.await {
                Err(e.into())
            } else {
                Ok(())
            }
        };

        (
            task,
            Outgoing {
                inner: outgoing,
                token_map: notify_or_indicate_handles,
            },
            events,
        )
    }
}

#[derive(Debug)]
pub struct Server {
    inner: AttServer,
}

impl Server {
    pub fn bind() -> io::Result<Self> {
        let server = AttServer::new()?;
        Ok(Self { inner: server })
    }

    pub async fn accept(&self) -> io::Result<Connection> {
        let connection = self.inner.accept().await?;
        Ok(Connection { inner: connection })
    }
}