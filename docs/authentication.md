## Authentication

### Overview
We use JWT tokens in communication between almost all components (compute, pageserver, safekeeper, CLI) regardless of the protocol used (HTTP/PostgreSQL).
storage_broker currently has no authentication.
Authentication is optional and is disabled by default for easier debugging.
It is used in some tests, though.
Note that we do not cover authentication with `pg.neon.tech` here.

For HTTP connections we use the Bearer authentication scheme.
For PostgreSQL connections we expect the token to be passed as a password.
There is a caveat for `psql`: it silently truncates passwords to 100 symbols, so to correctly pass JWT via `psql` you have to either use `PGPASSWORD` environment variable, or store password in `psql`'s config file.

Current token scopes are described in `utils::auth::Scope`.
There are no expiration or rotation schemes.

_TODO_: some scopes allow both access to server management API and to the data.
These probably should be split into multiple scopes.

Tokens should not occur in logs.
They may sometimes occur in configuration files, although this is discouraged
because configs may be parsed and dumped into logs.

#### Tokens generation and validation
JWT tokens are signed using a private key.
Compute/pageserver/safekeeper use the private key's public counterpart to validate JWT tokens.
These components should not have access to the private key and may only get tokens from their configuration or external clients. 

The key pair is generated once for an installation of compute/pageserver/safekeeper, e.g. by `neon_local init`.
There is currently no way to rotate the key without bringing down all components.

### CLI
CLI generates a key pair during call to `neon_local init` with the following commands:

```bash
openssl genrsa -out auth_private_key.pem 2048
openssl rsa -in auth_private_key.pem -pubout -outform PEM -out auth_public_key.pem
```

Configuration files for all components point to `public_key.pem` for JWT validation.
However, authentication is disabled by default.
There is no way to automatically enable it everywhere, you have to configure each component individually.

CLI also generates signed token (full access to Pageserver) and saves it in
the CLI's `config` file under `pageserver.auth_token`.
Note that pageserver's config does not have any similar parameter.
CLI is the only component which accesses that token.
Technically it could generate it from the private key on each run,
but it does not do that for some reason (_TODO_).

### Compute
#### Overview
Compute is a per-timeline PostgreSQL instance, so it should not have
any access to data of other tenants.
All tokens used by a compute are restricted to a specific tenant.
There is no auth isolation from other timelines of the same tenant,
but a non-rogue client never accesses another timeline even by an accident:
timeline IDs are random and hard to guess.

#### Incoming connections
All incoming connections are from PostgreSQL clients.
Their authentication is just plain PostgreSQL authentication and out of scope for this document.

There is no administrative API except those provided by PostgreSQL.

#### Outgoing connections
Compute connects to Pageserver for getting pages.
The connection string is configured by the `neon.pageserver_connstring` PostgreSQL GUC, e.g. `postgresql://no_user:$NEON_AUTH_TOKEN@localhost:15028`.
The environment variable inside the connection string is substituted with
the JWT token.

Compute connects to Safekeepers to write and commit data.
The token is the same for all safekeepers.
It's stored in an environment variable, whose name is configured
by the `neon.safekeeper_token_env` PostgreSQL GUC.
If the GUC is unset, no token is passed.

Note that both tokens can be (and typically are) the same;
the scope is the tenant and the token is usually passed through the
`$NEON_AUTH_TOKEN` environment variable.

### Pageserver
#### Overview
Pageserver keeps track of multiple tenants, each having multiple timelines.
For each timeline, it connects to the corresponding Safekeeper.
Information about "corresponding Safekeeper" is published by Safekeepers
in the storage_broker, but they do not publish access tokens, otherwise what is
the point of authentication.

Pageserver keeps a connection to some set of Safekeepers, which
may or may not correspond to active Computes.
Hence, we cannot obtain a per-timeline access token from a Compute.
E.g. if the timeline's Compute terminates before all WAL is
consumed by the Pageserver, the Pageserver continues consuming WAL.

Pageserver replicas' authentication is the same as the main's.

#### Incoming connections
Pageserver listens for connections from computes.
Each compute should present a token valid for the timeline's tenant.

Pageserver also has HTTP API: some parts are per-tenant,
some parts are server-wide, these are different scopes.

The `auth_type` configuration variable in Pageserver's config may have
either of three values:

* `Trust` removes all authentication. The outdated `MD5` value does likewise
* `NeonJWT` enables JWT validation.
   Tokens are validated using the public key which lies in a PEM file
   specified in the `auth_validation_public_key_path` config.

#### Outgoing connections
Pageserver makes a connection to a Safekeeper for each active timeline.
As Pageserver may want to access any timeline it has on the disk,
it is given a blanket JWT token to access any data on any Safekeeper.
This token is passed through an environment variable called `NEON_AUTH_TOKEN`
(non-configurable as of writing this text).

A better way _may be_ to store JWT token for each timeline next to it,
but may be not.

### Safekeeper
#### Overview
Safekeeper keeps track of multiple tenants, each having multiple timelines.

#### Incoming connections
Safekeeper accepts connections from Compute/Pageserver, each
connection corresponds to a specific timeline and requires
a corresponding JWT token.

Safekeeper also has HTTP API: some parts are per-tenant,
some parts are server-wide, these are different scopes.

The `auth-validation-public-key-path` command line options controls
the authentication mode:

* If the option is missing, there is no authentication or JWT token validation.
* If the option is present, it should be a path to the public key PEM file used for JWT token validation.

#### Outgoing connections
No connections are initiated by a Safekeeper.

### In the source code
Tests do not use authentication by default.
If you need it, you can enable it by configuring the test's environment:

```python
neon_env_builder.auth_enabled = True
```

You will have to generate tokens if you want to access components inside the test directly,
use `AuthKeys.generate_*_token` methods for that.
If you create a new scope, please create a new method to prevent mistypes in scope's name.
