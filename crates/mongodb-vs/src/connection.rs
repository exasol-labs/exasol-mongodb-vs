use exasol_udf_sdk::connect_back::ConnectionObject;
use exasol_udf_sdk::context::UdfContext;
use exasol_udf_sdk::error::UdfError;
use mongodb::Client;
use mongodb::options::{ClientOptions, Credential};

/// Resolve a named Exasol CONNECTION. The returned object must remain in
/// process memory and must never be serialized into adapter SQL or scan specs.
pub fn resolve(ctx: &dyn UdfContext, name: &str) -> Result<ConnectionObject, UdfError> {
    if name.trim().is_empty() {
        return Err(UdfError::User(
            "property 'MONGODB_CONNECTION' must not be empty".into(),
        ));
    }
    ctx.connection(name).map_err(|_| {
        UdfError::User(format!(
            "could not resolve Exasol CONNECTION '{name}'; verify that it exists and is accessible"
        ))
    })
}

/// Build a MongoDB client from a resolved CONNECTION without including its
/// address, username, or password in any returned error.
pub async fn client(conn: &ConnectionObject) -> Result<Client, UdfError> {
    let mut options = ClientOptions::parse(&conn.address).await.map_err(|_| {
        UdfError::User("MongoDB CONNECTION address is not a valid MongoDB URI".into())
    })?;

    if !conn.user.is_empty() || !conn.password.is_empty() {
        options.credential = Some(
            Credential::builder()
                .username((!conn.user.is_empty()).then(|| conn.user.clone()))
                .password((!conn.password.is_empty()).then(|| conn.password.clone()))
                .build(),
        );
    }

    Client::with_options(options)
        .map_err(|_| UdfError::User("failed to initialize the MongoDB client".into()))
}

#[cfg(test)]
mod tests {
    use exasol_udf_sdk::value::Value;

    use super::*;

    struct Context(Option<ConnectionObject>);

    impl UdfContext for Context {
        fn num_columns(&self) -> usize {
            0
        }
        fn get(&self, _col: usize) -> Result<&Value, UdfError> {
            unreachable!()
        }
        fn emit(&mut self, _values: &[Value]) -> Result<(), UdfError> {
            unreachable!()
        }
        fn next(&mut self) -> Result<bool, UdfError> {
            unreachable!()
        }
        fn connection(&self, _name: &str) -> Result<ConnectionObject, UdfError> {
            self.0
                .clone()
                .ok_or_else(|| UdfError::User("not found".into()))
        }
    }

    fn connection(address: &str, user: &str, password: &str) -> ConnectionObject {
        ConnectionObject {
            kind: String::new(),
            address: address.into(),
            user: user.into(),
            password: password.into(),
        }
    }

    #[test]
    fn resolves_named_connection_and_reports_safe_errors() {
        let expected = connection("mongodb://localhost:27017", "", "");
        let resolved = resolve(&Context(Some(expected)), "MONGO").unwrap();
        assert_eq!(resolved.address, "mongodb://localhost:27017");
        assert!(
            resolve(&Context(None), "MONGO")
                .unwrap_err()
                .to_string()
                .contains("MONGO")
        );
        assert!(
            resolve(&Context(None), "  ")
                .unwrap_err()
                .to_string()
                .contains("must not be empty")
        );
    }

    #[test]
    fn builds_clients_with_and_without_overridden_credentials() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            assert!(
                client(&connection("mongodb://localhost:27017", "", ""))
                    .await
                    .is_ok()
            );
            assert!(
                client(&connection("mongodb://localhost:27017", "alice", "secret"))
                    .await
                    .is_ok()
            );
            let marker = "do-not-echo";
            let error = client(&connection(&format!("not a uri {marker}"), "", ""))
                .await
                .unwrap_err()
                .to_string();
            assert!(!error.contains(marker));
        });
    }
}
