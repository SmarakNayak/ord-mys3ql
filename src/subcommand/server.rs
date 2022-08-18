use super::*;

use {
  self::{
    deserialize_ordinal_from_str::DeserializeOrdinalFromStr,
    templates::{
      block::BlockHtml, home::HomeHtml, ordinal::OrdinalHtml, output::OutputHtml, range::RangeHtml,
      transaction::TransactionHtml, Content,
    },
    tls_acceptor::TlsAcceptor,
  },
  axum::{body, http::header, response::Response},
  clap::ArgGroup,
  rust_embed::RustEmbed,
  rustls_acme::{
    acme::{ACME_TLS_ALPN_NAME, LETS_ENCRYPT_PRODUCTION_DIRECTORY, LETS_ENCRYPT_STAGING_DIRECTORY},
    caches::DirCache,
    AcmeConfig,
  },
  serde::{de, Deserializer},
  std::cmp::Ordering,
  tokio_stream::StreamExt,
};

mod deserialize_ordinal_from_str;
mod templates;
mod tls_acceptor;

#[derive(RustEmbed)]
#[folder = "static"]
struct StaticAssets;

struct StaticHtml {
  title: &'static str,
  html: &'static str,
}

impl Content for StaticHtml {
  fn title(&self) -> String {
    self.title.into()
  }
}

impl Display for StaticHtml {
  fn fmt(&self, f: &mut Formatter) -> fmt::Result {
    f.write_str(self.html)
  }
}

#[derive(Debug, Parser)]
#[clap(group = ArgGroup::new("port").multiple(false))]
pub(crate) struct Server {
  #[clap(
    long,
    default_value = "0.0.0.0",
    help = "Listen on <ADDRESS> for incoming requests."
  )]
  address: String,
  #[clap(
    long,
    help = "Request ACME TLS certificate for <ACME_DOMAIN>. This ord instance must be reachable at <ACME_DOMAIN>:443 to respond to Let's Encrypt ACME challenges."
  )]
  acme_domain: Vec<String>,
  #[clap(
    long,
    group = "port",
    help = "Listen on <HTTP_PORT> for incoming HTTP requests. Defaults to 80."
  )]
  http_port: Option<u16>,
  #[clap(
    long,
    group = "port",
    help = "Listen on <HTTPS_PORT> for incoming HTTPS requests.",
    requires_all = &["acme-cache", "acme-domain", "acme-contact"]
  )]
  https_port: Option<u16>,
  #[structopt(long, help = "Store ACME TLS certificates in <ACME_CACHE>.")]
  acme_cache: Option<PathBuf>,
  #[structopt(long, help = "Provide ACME contact <ACME_CONTACT>.")]
  acme_contact: Vec<String>,
}

impl Server {
  pub(crate) fn run(self, options: Options) -> Result {
    Runtime::new()?.block_on(async {
      let index = Arc::new(Index::open(&options)?);

      let clone = index.clone();
      thread::spawn(move || loop {
        if let Err(error) = clone.index_ranges() {
          log::error!("{error}");
        }
        thread::sleep(Duration::from_millis(100));
      });

      let app = Router::new()
        .route("/", get(Self::home))
        .route("/block/:hash", get(Self::block))
        .route("/bounties", get(Self::bounties))
        .route("/faq", get(Self::faq))
        .route("/height", get(Self::height))
        .route("/ordinal/:ordinal", get(Self::ordinal))
        .route("/output/:output", get(Self::output))
        .route("/range/:start/:end", get(Self::range))
        .route("/static/*path", get(Self::static_asset))
        .route("/status", get(Self::status))
        .route("/tx/:txid", get(Self::transaction))
        .layer(extract::Extension(index))
        .layer(
          CorsLayer::new()
            .allow_methods([http::Method::GET])
            .allow_origin(Any),
        );

      let port = self.port();

      let addr = (self.address.as_str(), port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("Failed to get socket addrs"))?;

      let handle = Handle::new();

      LISTENERS.lock().unwrap().push(handle.clone());

      let server = axum_server::Server::bind(addr).handle(handle);

      match self.acceptor() {
        Some(acceptor) => {
          server
            .acceptor(acceptor)
            .serve(app.into_make_service())
            .await?
        }
        None => server.serve(app.into_make_service()).await?,
      }

      Ok(())
    })
  }

  fn port(&self) -> u16 {
    self.http_port.or(self.https_port).unwrap_or(80)
  }

  fn acceptor(&self) -> Option<TlsAcceptor> {
    if self.https_port.is_some() {
      let config = AcmeConfig::new(&self.acme_domain)
        .contact(&self.acme_contact)
        .cache_option(Some(DirCache::new(
          self.acme_cache.as_ref().unwrap().clone(),
        )))
        .directory(if cfg!(test) {
          LETS_ENCRYPT_STAGING_DIRECTORY
        } else {
          LETS_ENCRYPT_PRODUCTION_DIRECTORY
        });

      let mut state = config.state();

      let acceptor = state.acceptor();

      tokio::spawn(async move {
        while let Some(result) = state.next().await {
          match result {
            Ok(ok) => log::info!("ACME event: {:?}", ok),
            Err(err) => log::error!("ACME error: {:?}", err),
          }
        }
      });

      Some(TlsAcceptor(acceptor))
    } else {
      None
    }
  }

  async fn ordinal(
    index: extract::Extension<Arc<Index>>,
    extract::Path(DeserializeOrdinalFromStr(ordinal)): extract::Path<DeserializeOrdinalFromStr>,
  ) -> impl IntoResponse {
    match index.blocktime(ordinal.height()) {
      Ok(blocktime) => OrdinalHtml { ordinal, blocktime }.page().into_response(),
      Err(err) => {
        eprintln!("Failed to retrieve height from index: {err}");
        (
          StatusCode::INTERNAL_SERVER_ERROR,
          Html(
            StatusCode::INTERNAL_SERVER_ERROR
              .canonical_reason()
              .unwrap_or_default()
              .to_string(),
          ),
        )
          .into_response()
      }
    }
  }

  async fn output(
    index: extract::Extension<Arc<Index>>,
    extract::Path(outpoint): extract::Path<OutPoint>,
  ) -> impl IntoResponse {
    match index.list(outpoint) {
      Ok(Some(list)) => OutputHtml { outpoint, list }.page().into_response(),
      Ok(None) => (StatusCode::NOT_FOUND, Html("Output unknown.".to_string())).into_response(),
      Err(err) => {
        eprintln!("Error serving request for output: {err}");
        (
          StatusCode::INTERNAL_SERVER_ERROR,
          Html(
            StatusCode::INTERNAL_SERVER_ERROR
              .canonical_reason()
              .unwrap_or_default()
              .to_string(),
          ),
        )
          .into_response()
      }
    }
  }

  async fn range(
    extract::Path((DeserializeOrdinalFromStr(start), DeserializeOrdinalFromStr(end))): extract::Path<
      (DeserializeOrdinalFromStr, DeserializeOrdinalFromStr),
    >,
  ) -> impl IntoResponse {
    match start.cmp(&end) {
      Ordering::Equal => (StatusCode::BAD_REQUEST, Html("Empty Range".to_string())).into_response(),
      Ordering::Greater => (
        StatusCode::BAD_REQUEST,
        Html("Range Start Greater Than Range End".to_string()),
      )
        .into_response(),
      Ordering::Less => RangeHtml { start, end }.page().into_response(),
    }
  }

  async fn home(index: extract::Extension<Arc<Index>>) -> impl IntoResponse {
    match index.blocks(100) {
      Ok(blocks) => HomeHtml::new(blocks).page().into_response(),
      Err(err) => {
        eprintln!("Error getting blocks: {err}");
        (
          StatusCode::INTERNAL_SERVER_ERROR,
          Html(
            StatusCode::INTERNAL_SERVER_ERROR
              .canonical_reason()
              .unwrap_or_default()
              .to_string(),
          ),
        )
          .into_response()
      }
    }
  }

  async fn block(
    extract::Path(hash): extract::Path<sha256d::Hash>,
    index: extract::Extension<Arc<Index>>,
  ) -> impl IntoResponse {
    match index.block_with_hash(hash) {
      Ok(Some(block)) => BlockHtml::new(block).page().into_response(),
      Ok(None) => (
        StatusCode::NOT_FOUND,
        Html(
          StatusCode::NOT_FOUND
            .canonical_reason()
            .unwrap_or_default()
            .to_string(),
        ),
      )
        .into_response(),
      Err(error) => {
        eprintln!("Error serving request for block with hash {hash}: {error}");
        (
          StatusCode::INTERNAL_SERVER_ERROR,
          Html(
            StatusCode::INTERNAL_SERVER_ERROR
              .canonical_reason()
              .unwrap_or_default()
              .to_string(),
          ),
        )
          .into_response()
      }
    }
  }

  async fn transaction(
    index: extract::Extension<Arc<Index>>,
    extract::Path(txid): extract::Path<Txid>,
  ) -> impl IntoResponse {
    match index.transaction(txid) {
      Ok(Some(transaction)) => TransactionHtml::new(transaction).page().into_response(),
      Ok(None) => (
        StatusCode::NOT_FOUND,
        Html(
          StatusCode::NOT_FOUND
            .canonical_reason()
            .unwrap_or_default()
            .to_string(),
        ),
      )
        .into_response(),
      Err(error) => {
        eprintln!("Error serving request for transaction with txid {txid}: {error}");
        (
          StatusCode::INTERNAL_SERVER_ERROR,
          Html(
            StatusCode::INTERNAL_SERVER_ERROR
              .canonical_reason()
              .unwrap_or_default()
              .to_string(),
          ),
        )
          .into_response()
      }
    }
  }

  async fn status() -> impl IntoResponse {
    (
      StatusCode::OK,
      StatusCode::OK
        .canonical_reason()
        .unwrap_or_default()
        .to_string(),
    )
  }

  async fn static_asset(extract::Path(path): extract::Path<String>) -> impl IntoResponse {
    match StaticAssets::get(if let Some(stripped) = path.strip_prefix('/') {
      stripped
    } else {
      &path
    }) {
      Some(content) => {
        let body = body::boxed(body::Full::from(content.data));
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        Response::builder()
          .header(header::CONTENT_TYPE, mime.as_ref())
          .body(body)
          .unwrap()
      }
      None => (
        StatusCode::NOT_FOUND,
        Html(
          StatusCode::NOT_FOUND
            .canonical_reason()
            .unwrap_or_default()
            .to_string(),
        ),
      )
        .into_response(),
    }
  }

  async fn height(index: extract::Extension<Arc<Index>>) -> impl IntoResponse {
    match index.height() {
      Ok(height) => (StatusCode::OK, Html(format!("{}", height))),
      Err(err) => {
        eprintln!("Failed to retrieve height from index: {err}");
        (
          StatusCode::INTERNAL_SERVER_ERROR,
          Html(
            StatusCode::INTERNAL_SERVER_ERROR
              .canonical_reason()
              .unwrap_or_default()
              .to_string(),
          ),
        )
      }
    }
  }

  async fn faq() -> impl IntoResponse {
    StaticHtml {
      title: "Ordinal FAQ",
      html: include_str!(concat!(env!("OUT_DIR"), "/faq.html")),
    }
    .page()
    .into_response()
  }

  async fn bounties() -> impl IntoResponse {
    StaticHtml {
      title: "Ordinal Bounties",
      html: include_str!(concat!(env!("OUT_DIR"), "/bounties.html")),
    }
    .page()
    .into_response()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn port_defaults_to_80() {
    match Arguments::try_parse_from(&["ord", "server"])
      .unwrap()
      .subcommand
    {
      Subcommand::Server(server) => assert_eq!(server.port(), 80),
      subcommand => panic!("Unexpected subcommand: {subcommand:?}"),
    }
  }

  #[test]
  fn http_and_https_port_conflict() {
    let err = Arguments::try_parse_from(&["ord", "server", "--http-port=0", "--https-port=0"])
      .unwrap_err()
      .to_string();

    assert!(
      err.starts_with("error: The argument '--http-port <HTTP_PORT>' cannot be used with '--https-port <HTTPS_PORT>'\n"),
      "{}",
      err
    );
  }

  #[test]
  fn http_port_requires_acme_flags() {
    let err = Arguments::try_parse_from(&["ord", "server", "--https-port=0"])
      .unwrap_err()
      .to_string();

    assert!(
      err.starts_with("error: The following required arguments were not provided:\n    --acme-cache <ACME_CACHE>\n    --acme-domain <ACME_DOMAIN>\n    --acme-contact <ACME_CONTACT>\n"),
      "{}",
      err
    );
  }

  #[test]
  fn acme_contact_accepts_multiple_values() {
    assert!(Arguments::try_parse_from(&[
      "ord",
      "server",
      "--address",
      "127.0.0.1",
      "--http-port",
      "0",
      "--acme-contact",
      "foo",
      "--acme-contact",
      "bar"
    ])
    .is_ok());
  }

  #[test]
  fn acme_domain_accepts_multiple_values() {
    assert!(Arguments::try_parse_from(&[
      "ord",
      "server",
      "--address",
      "127.0.0.1",
      "--http-port",
      "0",
      "--acme-domain",
      "foo",
      "--acme-domain",
      "bar"
    ])
    .is_ok());
  }
}