//! Websockets used by the server to communicate with the clients.

use crate::base::*;

/// Creates a websocket server at some address.
fn new_server(addr: &str, port: usize) -> Res<Server> {
    let server = Server::bind(&format!("{}:{}", addr, port))
        .chain_err(|| format!("while binding websocket server at `{}:{}`", addr, port))?;
    Ok(server)
}

fn handle_requests(server: Server) -> Res<()> {
    for request in server.filter_map(Result::ok) {
        let mut handler = Handler::new(request).chain_err(|| "while creating request handler")?;
        std::thread::spawn(move || handler.run());
        ()
    }
    Ok(())
}

pub fn spawn_server(addr: &str, port: usize) -> Res<()> {
    let server = new_server(addr, port)?;
    std::thread::spawn(move || handle_requests(server));
    Ok(())
}

pub struct Handler {
    /// Ip address of the client.
    ip: IpAddr,
    /// Receives messages from the client.
    recver: Receiver,
    /// Sends messages to the client.
    sender: Sender,
    /// The charts of the client.
    charts: Charts,
    /// Stores the result of receiving messages from the client.
    from_client: FromClient,
    /// Time at which we last sent points to render.
    last_frame: Instant,
    /// Minimum time between two rendering steps.
    frame_span: Duration,
}

impl Handler {
    /// Constructor from a request and a dump directory.
    pub fn new(request: Request) -> Res<Self> {
        let client = request
            .accept()
            .map_err(|(_, e)| e)
            .chain_err(|| "while accepting websocket connection")?;
        let ip = client
            .peer_addr()
            .chain_err(|| "while retrieving client's IP address")?;

        let (recver, sender) = client
            .split()
            .chain_err(|| "while splitting the client into receive/send pair")?;

        let slf = Handler {
            ip,
            recver,
            sender,
            charts: Charts::new(),
            from_client: FromClient::new(),
            last_frame: Instant::now(),
            frame_span: Duration::from_millis(1_000),
        };

        Ok(slf)
    }

    /// Runs the handler.
    pub fn run(&mut self) {
        unwrap!(self.internal_run())
    }

    /// Sets the time of the last frame to now.
    fn set_last_frame(&mut self) {
        self.last_frame = Instant::now()
    }

    /// Runs the handler, can fail.
    fn internal_run(&mut self) -> Res<()> {
        self.set_last_frame();
        self.init()?;

        // Let's do this.
        loop {
            // Receive new messages.
            self.receive_messages()?;

            // Connection closed?
            if self.from_client.is_closed() {
                let close_data = self
                    .from_client
                    .close_data()
                    .map(
                        |CloseData {
                             status_code,
                             reason,
                         }| {
                            format!("status code `{}`: {}", status_code, reason)
                        },
                    )
                    .unwrap_or_else(|| "no information".into());
                log!(self.ip => "client closed the connection with {}", close_data);
                break;
            }

            // Handle the messages.
            for msg in self.from_client.drain() {
                self.charts.handle_msg(msg)?
            }

            // Wait before rendering if necessary.
            let now = Instant::now();
            if now <= self.last_frame + self.frame_span {
                std::thread::sleep((self.last_frame + self.frame_span) - now)
            }

            // Render.
            let points = self
                .charts
                .new_points(false)
                .chain_err(|| "while constructing points for the client")?;
            self.send(msg::to_client::ChartsMsg::new_points(points))
                .chain_err(|| "while sending points to the client")?;
        }

        Ok(())
    }

    /// Initializes a client.
    pub fn init(&mut self) -> Res<()> {
        let points = self
            .charts
            .new_points(true)
            .chain_err(|| "while constructing points for client init")?;
        log!(self.ip => "sending points to client");
        self.send(msg::to_client::ChartsMsg::new_points(points))
            .chain_err(|| "while sending points for client init")?;
        Ok(())
    }

    /// Sends a message to the client.
    pub fn send<Msg>(&mut self, msg: Msg) -> Res<()>
    where
        Msg: Into<msg::to_client::Msg>,
    {
        use websocket::message::OwnedMessage;

        let content = msg
            .into()
            .as_json()
            .chain_err(|| "while encoding message as toml")?
            .into_bytes();
        let msg = OwnedMessage::Binary(content);
        self.sender
            .send_message(&msg)
            .chain_err(|| format!("while sending message to client {}", self.ip))?;
        Ok(())
    }

    /// Retrieves actions to perform from the client before rendering.
    ///
    /// Returns `None` if the client requested to close
    fn receive_messages(&mut self) -> Res<()> {
        // Used in the `match` below.
        use websocket::message::OwnedMessage::*;

        for message in self.recver.incoming_messages() {
            let message = message.chain_err(|| "while retrieving message")?;

            // Let's do this.
            match message {
                // Normal message(s) from the client.
                Text(text) => {
                    let msg = msg::from_client::Msg::from_json(&text)
                        .chain_err(|| "while parsing message from client")?;
                    self.from_client.push(msg)?
                }
                Binary(data) => {
                    let msg = msg::from_client::Msg::from_json_bytes(&data)
                        .chain_err(|| "while parsing message from client")?;
                    self.from_client.push(msg)?
                }

                // The client is telling us to stop listening for messages and render.
                Pong(_) => break,

                // Client is closing the connection.
                Close(close_data) => {
                    self.from_client.close()?;
                    self.from_client.set_close_data(close_data)?;
                    break;
                }

                // Unexpected mesage(s).
                Ping(label) => bail!(
                    "unexpected `Ping({})` message",
                    String::from_utf8_lossy(&label)
                ),
            }
        }

        Ok(())
    }
}
