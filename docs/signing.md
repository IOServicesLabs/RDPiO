# Signing

rdpio's releases carry two kinds of signature.

| What | Proves | Key |
|---|---|---|
| **Update manifest** (`rdpio.exe.update`) | This build is an official rdpio release; rdpio installs updates only with a valid one | rdpio's release key (Ed25519). Secret `RDPIO_SIGNING_KEY`; the private half is kept offline |
| **Authenticode** (on `rdpio.exe`) | To Windows: who published it. No SmartScreen "unknown publisher" warning, and a verified administrator prompt | Azure Artifact Signing (formerly Trusted Signing) |

The release workflow (`.github/workflows/release.yml`) does both:

1. It Authenticode-signs `rdpio.exe`.
2. Then it writes the update manifest for the signed exe.

Without the Artifact Signing settings, the exe goes out unsigned with a warning in the build log. Everything else still works.

## Setting up Azure Artifact Signing (once)

This costs about $10 a month (the Basic tier). Steps 1–5 happen in the
[Azure portal](https://portal.azure.com).

1. **Subscription.** Sign in, or create an Azure account, and have a
   pay-as-you-go subscription. Under **Subscriptions → your subscription →
   Resource providers**, register **Microsoft.CodeSigning**.
2. **Signing account.** Search for **Artifact Signing** (older portal views:
   *Trusted Signing*). Choose **Create**, then:
   - pick a resource group (a new `rdpio-signing` is fine);
   - give the account a name, for example `ioservices-signing`;
   - choose a region, for example **East US**;
   - choose the **Basic** tier.

   Its **Overview** page shows the account URI, such as
   `https://eus.codesigning.azure.net/`. That is the endpoint.
3. **Identity validation.** This is the part that takes days. On the account,
   open **Access control (IAM)** and give yourself the **Artifact Signing
   Identity Verifier** role. Then open **Identity validation → New** and
   choose **Public**:
   - **Organization:** your legal business name and address and its tax ID,
     with an email at the business's domain. Microsoft checks the business's
     records and may ask for documents.
   - **Individual:** available in the US and Canada. This is the route if
     the organization can't be verified yet.

   Wait for the status to read **Completed**.
4. **Certificate profile.** Open **Certificate profiles → Create → Public
   Trust**. Pick the validated identity and name the profile, for example
   `rdpio`.
5. **An identity for GitHub.** Open **Microsoft Entra ID → App registrations
   → New registration** and name it `rdpio-github-signing`.
   - Under **Certificates & secrets**, make a new client secret. It can last
     up to 24 months; note when it expires.
   - Copy the secret's **Value**, the **Application (client) ID** and the
     **Directory (tenant) ID**.
   - Back on the signing account, open **Access control (IAM) → Add role
     assignment**. Choose **Artifact Signing Certificate Profile Signer** and
     select the `rdpio-github-signing` app as the member.
6. **GitHub.** In the repository, open **Settings → Secrets and variables →
   Actions**.
   - Add these **secrets**:
     - `AZURE_TENANT_ID`: the directory (tenant) ID;
     - `AZURE_CLIENT_ID`: the application (client) ID;
     - `AZURE_CLIENT_SECRET`: the secret's value.
   - Add these **variables**:
     - `ARTIFACT_SIGNING_ENDPOINT`: the account URI from step 2;
     - `ARTIFACT_SIGNING_ACCOUNT`: the account name;
     - `ARTIFACT_SIGNING_PROFILE`: the certificate profile name.
7. **Check.** Push to `main`. The nightly build's log shows `Signed by:
   CN=<your validated name>`. On a PC, open the downloaded `rdpio.exe` →
   **Properties → Digital Signatures**.

Renew the client secret (step 5) before it expires and update
`AZURE_CLIENT_SECRET`, or signing stops and builds go out unsigned again.

## The display driver

rdpio Display (`drivers/rdpio-display`) is a user-mode driver, so it doesn't
need Microsoft's own signature. A driver package still has to be signed to
install without test mode, and the same Artifact Signing account can sign
it. Until it ships signed, hosts can use the open-source Virtual Display
Driver (`winget install VirtualDrivers.Virtual-Display-Driver`), which rdpio
uses automatically.
