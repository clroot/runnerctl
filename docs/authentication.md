# Authentication

Credentials belong to named profiles, and each pool selects a profile with `--auth`. Different profiles can serve different organizations or repositories. The manager exchanges credentials for short-lived registration tokens; neither PATs nor App private keys are injected into runners.

## Personal access tokens

```bash
runnerctl auth login --profile team
# Hidden prompt; no token in shell history or process arguments.
```

Fine-grained PAT permissions:

| Pool scope | Required permission |
| --- | --- |
| Organization | Organization **Self-hosted runners: Read and write** |
| Repository | Repository **Administration: Read and write** |

Select the correct resource owner/repositories and obtain any organization approval. Classic PATs require `admin:org` for organization runners or `repo` for private repository runners. GitHub's [runner API documentation](https://docs.github.com/en/rest/actions/self-hosted-runners) is authoritative for permissions and token support.

For automation, feed a token through stdin (`--token-stdin`) or reference an existing environment variable:

```bash
runnerctl auth login --profile team --env TEAM_RUNNER_TOKEN
```

The variable must exist in the **manager's** environment. A systemd user service does not automatically inherit shell variables; file credentials are usually easier for services. New token files use mode 0600, and configuration contains only their references. Old unreferenced files are retained for deliberate cleanup after rotation.

## GitHub App installations

Create and install a GitHub App with organization **Self-hosted runners: Read and write** for organization pools, or repository **Administration: Read and write** for repository pools. The installation must include the intended repositories. No webhook is required for fixed pool capacity.

Save the generated RSA PEM private key on the manager host with mode 0600, then configure its identity:

```bash
chmod 600 /absolute/path/runner-app.pem
runnerctl auth app --profile team-app \
  --app-id YOUR_APP_ID_OR_CLIENT_ID \
  --installation-id YOUR_INSTALLATION_ID \
  --private-key /absolute/path/runner-app.pem
runnerctl pool add ci --org YOUR_ORG --auth team-app --labels ci
runnerctl doctor
```

The installation ID is the number in the App installation's GitHub settings URL. `--key-env VARIABLE` is an alternative to `--private-key`; that variable contains the PEM key, including newlines, and must be available to the manager.

The CLI validates the key and stores its reference. The manager signs RS256 JWTs and requests installation access tokens, reusing them until they are within one minute of expiry. Reconfiguring the profile clears cached credentials and retry state. Installation tokens and JWTs are not written to configuration, state, or runner environment variables.

`doctor` verifies runner read access. Registration write access is exercised when starting a runner. An App installation, account policy, or resource selection can still restrict access despite apparently correct App permissions.

See GitHub's [App JWT](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-json-web-token-jwt-for-a-github-app) and [installation access token](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-an-installation-access-token-for-a-github-app) documentation.
