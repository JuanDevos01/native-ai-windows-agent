<#
.SYNOPSIS
  Register the Azure AD (Entra) application Metis needs to read an Office 365
  mailbox over Microsoft Graph, and optionally write the result into
  ~/.metis/config.json.

.DESCRIPTION
  Office 365 cannot be reached over IMAP: Microsoft disabled Basic
  authentication for IMAP/POP/SMTP in Exchange Online on 2022-10-01, so
  Metis' IMAP backend gets "NO AUTHENTICATE failed" no matter how correct the
  credentials are. Graph with OAuth2 is the supported path, and it needs an
  app registration. This script does the registration end to end:

    1. installs the Microsoft.Graph PowerShell submodules (current user only)
    2. signs you in interactively
    3. creates the application + its service principal
    4. grants the Mail.ReadWrite / Mail.Send APPLICATION permissions
    5. grants admin consent
    6. creates a client secret
    7. restricts the app to ONE mailbox  (strongly recommended - see below)
    8. optionally writes tenant/client/secret into Metis' config.json

  WHAT THIS SCRIPT CANNOT DO FOR YOU
    - The sign-in itself. Step 2 opens a browser; you must authenticate
      (including MFA). No script can bypass that, by design.
    - Steps 4-5 require you to be Global Administrator or Privileged Role
      Administrator. Granting an application permission tenant-wide is an
      administrative decision, so a non-admin sign-in will fail there. If you
      are not an admin, run with -SkipConsent and send the printed summary to
      whoever is.

  SECURITY - PLEASE READ
    Mail.ReadWrite as an APPLICATION permission grants access to EVERY
    mailbox in the tenant, not just the one you name. Step 7 therefore scopes
    the app down to the single mailbox you specify, preferring RBAC for
    Applications (Microsoft's current mechanism) and falling back to the
    legacy Application Access Policy where RBAC is unavailable. Skipping it
    (-SkipMailboxRestriction) leaves a credential in config.json that can
    read the whole organisation's mail.

.PARAMETER Mailbox
  The mailbox Metis should read, e.g. info@yourdomain.com.

.PARAMETER AppName
  Display name for the app registration. Default "Metis Mail".

.PARAMETER SecretYears
  Client secret lifetime in years (1 or 2). Default 2.

.PARAMETER WriteConfig
  Write tenant/client/secret into ~/.metis/config.json (a timestamped backup
  is taken first).

.PARAMETER SkipConsent
  Create the app but do not attempt admin consent (for non-admin operators).

.PARAMETER SkipMailboxRestriction
  Do not scope the app to one mailbox. Not recommended - see SECURITY above.

.EXAMPLE
  .\setup-o365-graph.ps1 -Mailbox info@contoso.com -WriteConfig
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Mailbox,

    [string]$AppName = "Metis Mail",

    [ValidateRange(1, 2)]
    [int]$SecretYears = 2,

    [switch]$WriteConfig,
    [switch]$SkipConsent,
    [switch]$SkipMailboxRestriction,

    # Set automatically when the script relaunches itself after upgrading
    # PowerShellGet. Not meant to be passed by hand.
    [switch]$Bootstrapped
)

$ErrorActionPreference = "Stop"

function Write-Step { param([string]$Text) Write-Host "`n==> $Text" -ForegroundColor Cyan }
function Write-Ok   { param([string]$Text) Write-Host "    OK  $Text" -ForegroundColor Green }
function Write-Warn { param([string]$Text) Write-Host "    !   $Text" -ForegroundColor Yellow }

# Microsoft Graph's own app id - a fixed, well-known constant.
$GraphAppId = "00000003-0000-0000-c000-000000000000"
# Permission NAMES, not GUIDs: the ids are looked up from the tenant at run
# time so a mistyped/stale GUID cannot silently grant the wrong permission.
$WantedRoles = @("Mail.ReadWrite", "Mail.Send")

# Rebuild this script's own arguments, for the two places that hand over to a
# fresh PowerShell session.
function Format-Arg {
    # Start-Process joins -ArgumentList with spaces and does no quoting, so a
    # value containing one arrives as two tokens: the default app name
    # "Metis Mail" bound "Metis" to -AppName and left "Mail" as a stray
    # positional ("A positional parameter cannot be found..."). Paths under
    # "Program Files" break the same way.
    param([string]$Value)
    if ($Value -match '\s') { '"' + $Value + '"' } else { $Value }
}

function Get-ForwardArgs {
    param([switch]$MarkBootstrapped)
    $a = @('-NoExit', '-NoProfile', '-ExecutionPolicy', 'Bypass',
           '-File',        (Format-Arg $PSCommandPath),
           '-Mailbox',     (Format-Arg $Mailbox),
           '-AppName',     (Format-Arg $AppName),
           '-SecretYears', $SecretYears)
    if ($WriteConfig)            { $a += '-WriteConfig' }
    if ($SkipConsent)            { $a += '-SkipConsent' }
    if ($SkipMailboxRestriction) { $a += '-SkipMailboxRestriction' }
    if ($MarkBootstrapped)       { $a += '-Bootstrapped' }
    return $a
}

# ── 0. PowerShell edition ─────────────────────────────────────────────────
# Everything downstream depends on this. Windows PowerShell 5.1 ships
# PowerShellGet 1.0.0.1, which cannot install the Microsoft.Graph modules and
# fails *silently* while doing it. PowerShell 7 ships a current one, so check
# for it before anything else and either use it or say plainly what to install.
if ($PSVersionTable.PSEdition -eq 'Desktop') {
    $pwshCmd = Get-Command pwsh -ErrorAction SilentlyContinue
    if ($pwshCmd) {
        Write-Step "PowerShell 7 found - continuing there (5.1 cannot install the Graph modules)"
        Start-Process pwsh -ArgumentList (Get-ForwardArgs)
        Write-Ok "Continue in the new PowerShell 7 window - this one is done."
        return
    }

    Write-Host ""
    Write-Host "  ------------------------------------------------------------------" -ForegroundColor Yellow
    Write-Host "   PowerShell 7 is not installed." -ForegroundColor Yellow
    Write-Host ""
    Write-Host "   You are on Windows PowerShell $($PSVersionTable.PSVersion), which ships"
    Write-Host "   PowerShellGet 1.0.0.1 - too old to install the Microsoft.Graph"
    Write-Host "   modules this script needs."
    Write-Host ""
    Write-Host "   RECOMMENDED - install PowerShell 7, then re-run this script:" -ForegroundColor Cyan
    if (Get-Command winget -ErrorAction SilentlyContinue) {
        Write-Host "       winget install --id Microsoft.PowerShell --source winget" -ForegroundColor Cyan
    } else {
        Write-Host "       https://aka.ms/powershell-release?tag=stable" -ForegroundColor Cyan
    }
    Write-Host ""
    Write-Host "   Otherwise this script will upgrade PowerShellGet here and reopen"
    Write-Host "   a second window to finish. That works, but takes longer and"
    Write-Host "   changes your PowerShell module setup."
    Write-Host "  ------------------------------------------------------------------" -ForegroundColor Yellow
    Write-Host ""
    $ans = Read-Host "  Continue without PowerShell 7? [y/N]"
    if ($ans -notmatch '^(y|yes)$') {
        Write-Host ""
        Write-Ok "Stopped. Install PowerShell 7 with the command above, then run this script again."
        return
    }
}

# ── 1. Modules ────────────────────────────────────────────────────────────
# Windows ships PowerShell 5.1 with PowerShellGet 1.0.0.1 (2016). That version
# cannot install the Microsoft.Graph modules: Install-Module returns without
# error and installs nothing, so the next Import-Module fails with "no valid
# module file was found". It has to be upgraded first, and a module upgrade
# only takes effect in a NEW session - hence the one-time relaunch below.
Write-Step "Checking PowerShell modules"

# Older PowerShell defaults to TLS 1.0, which the gallery refuses.
try {
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {}

# -ForceBootstrap answers the "Would you like PackageManagement to install
# 'nuget' now?" prompt; -Force alone does not suppress it.
if (-not (Get-PackageProvider -Name NuGet -ErrorAction SilentlyContinue)) {
    Write-Warn "Installing the NuGet package provider"
    Install-PackageProvider -Name NuGet -MinimumVersion 2.8.5.201 `
        -Scope CurrentUser -Force -ForceBootstrap | Out-Null
    # Load it into THIS session; a provider installed mid-session is not
    # picked up automatically, which makes the next Install-Module a no-op.
    Import-PackageProvider -Name NuGet -Force | Out-Null
}

$gallery = Get-PSRepository -Name PSGallery -ErrorAction SilentlyContinue
if ($gallery -and $gallery.InstallationPolicy -ne 'Trusted') {
    Write-Warn "Trusting PSGallery for this user so module installs do not prompt"
    Set-PSRepository -Name PSGallery -InstallationPolicy Trusted
}

$pgVersion = (Get-Module PowerShellGet -ListAvailable |
              Sort-Object Version -Descending | Select-Object -First 1).Version
Write-Ok "PowerShell $($PSVersionTable.PSVersion), PowerShellGet $pgVersion"

if ($pgVersion -lt [version]'2.0.0') {
    if ($Bootstrapped) {
        throw ("PowerShellGet is still $pgVersion after an upgrade attempt. " +
               "Install it manually with: Install-Module PowerShellGet -Force -AllowClobber " +
               "-Scope CurrentUser -SkipPublisherCheck, then reopen PowerShell and re-run this script.")
    }
    Write-Warn "PowerShellGet $pgVersion is too old to install the Microsoft.Graph modules - upgrading"
    Install-Module PowerShellGet -MinimumVersion 2.2.5 -Scope CurrentUser `
        -Force -AllowClobber -SkipPublisherCheck -Confirm:$false
    Write-Ok "PowerShellGet upgraded"

    # The upgrade is only visible to a fresh session, so start one and hand
    # over. Forwarding the original arguments keeps this invisible to the user.
    Write-Step "Reopening PowerShell to finish setup (the old session cannot see the upgrade)"
    Start-Process powershell -ArgumentList (Get-ForwardArgs -MarkBootstrapped)
    Write-Ok "Continue in the new window - this one is done."
    return
}

$needed = @("Microsoft.Graph.Authentication", "Microsoft.Graph.Applications")
foreach ($m in $needed) {
    if (-not (Get-Module -ListAvailable -Name $m)) {
        Write-Warn "$m not found - installing for the current user (no admin rights needed)"
        Install-Module $m -Scope CurrentUser -Force -AllowClobber -Confirm:$false
    }
    # Verify rather than trust: a silent no-op here is exactly the failure
    # this section exists to prevent.
    if (-not (Get-Module -ListAvailable -Name $m)) {
        throw ("$m did not install. PowerShell $($PSVersionTable.PSVersion), " +
               "PowerShellGet $pgVersion, module path '$($env:PSModulePath -split ';' | Select-Object -First 1)'. " +
               "Try: Install-Module $m -Scope CurrentUser -Force -AllowClobber -Verbose")
    }
    Import-Module $m -ErrorAction Stop
    Write-Ok $m
}

# ── 2. Sign in ────────────────────────────────────────────────────────────
Write-Step "Signing in to Microsoft Graph (a browser window will open)"
$scopes = @(
    "Application.ReadWrite.All",
    "AppRoleAssignment.ReadWrite.All",
    "Directory.Read.All"
)
Connect-MgGraph -Scopes $scopes -NoWelcome
$ctx = Get-MgContext
if (-not $ctx) { throw "Sign-in failed." }
$TenantId = $ctx.TenantId
Write-Ok "Signed in as $($ctx.Account) - tenant $TenantId"

# ── 3. Application + service principal ────────────────────────────────────
Write-Step "Creating the app registration '$AppName'"
$existing = Get-MgApplication -Filter "displayName eq '$AppName'" -ErrorAction SilentlyContinue
if ($existing) {
    $app = $existing | Select-Object -First 1
    Write-Warn "An app named '$AppName' already exists - reusing it (appId $($app.AppId))"
} else {
    $app = New-MgApplication -DisplayName $AppName -SignInAudience "AzureADMyOrg"
    Write-Ok "Created app - appId $($app.AppId)"
}

$sp = Get-MgServicePrincipal -Filter "appId eq '$($app.AppId)'" -ErrorAction SilentlyContinue
if (-not $sp) {
    $sp = New-MgServicePrincipal -AppId $app.AppId
    Write-Ok "Created service principal"
} else {
    Write-Ok "Service principal already present"
}

# ── 4. Permissions ────────────────────────────────────────────────────────
Write-Step "Resolving Graph application permissions"
$graphSp = Get-MgServicePrincipal -Filter "appId eq '$GraphAppId'"
$roles = @()
foreach ($name in $WantedRoles) {
    $role = $graphSp.AppRoles | Where-Object { $_.Value -eq $name -and $_.AllowedMemberTypes -contains "Application" }
    if (-not $role) { throw "Could not find application permission '$name' on Microsoft Graph." }
    $roles += $role
    Write-Ok "$name -> $($role.Id)"
}

Write-Step "Attaching the permissions to the app"
$access = @{
    ResourceAppId  = $GraphAppId
    ResourceAccess = @($roles | ForEach-Object { @{ Id = $_.Id; Type = "Role" } })
}
Update-MgApplication -ApplicationId $app.Id -RequiredResourceAccess @($access)
Write-Ok "Permissions requested"

# ── 5. Admin consent ──────────────────────────────────────────────────────
if ($SkipConsent) {
    Write-Warn "Skipping admin consent (-SkipConsent). An administrator must grant it before Metis can connect."
} else {
    Write-Step "Granting admin consent (requires Global Admin / Privileged Role Admin)"
    foreach ($role in $roles) {
        $already = Get-MgServicePrincipalAppRoleAssignment -ServicePrincipalId $sp.Id -ErrorAction SilentlyContinue |
                   Where-Object { $_.AppRoleId -eq $role.Id -and $_.ResourceId -eq $graphSp.Id }
        if ($already) { Write-Ok "$($role.Value) already consented"; continue }
        try {
            New-MgServicePrincipalAppRoleAssignment -ServicePrincipalId $sp.Id `
                -PrincipalId $sp.Id -ResourceId $graphSp.Id -AppRoleId $role.Id | Out-Null
            Write-Ok "Consented $($role.Value)"
        } catch {
            Write-Warn "Could not consent $($role.Value): $($_.Exception.Message)"
            Write-Warn "You are probably not an admin. Re-run as one, or have an admin approve at:"
            Write-Warn "  https://login.microsoftonline.com/$TenantId/adminconsent?client_id=$($app.AppId)"
        }
    }
}

# ── 6. Client secret ──────────────────────────────────────────────────────
Write-Step "Creating a client secret (valid $SecretYears year(s))"
$cred = Add-MgApplicationPassword -ApplicationId $app.Id -PasswordCredential @{
    DisplayName = "Metis $(Get-Date -Format yyyy-MM-dd)"
    EndDateTime = (Get-Date).AddYears($SecretYears)
}
$Secret = $cred.SecretText
Write-Ok "Secret created - it is shown only once, below"

# ── 7. Restrict to one mailbox ────────────────────────────────────────────
# Two mechanisms exist. RBAC for Applications is the current one; Application
# Access Policies are the legacy mechanism that Microsoft is replacing and
# advises against for new configuration. We try RBAC first and fall back.
if ($SkipMailboxRestriction) {
    Write-Warn "SKIPPING mailbox restriction. This app can currently read EVERY mailbox in the tenant."
} else {
    Write-Step "Restricting the app to $Mailbox only"
    if (-not (Get-Module -ListAvailable -Name ExchangeOnlineManagement)) {
        Write-Warn "ExchangeOnlineManagement not found - installing for the current user"
        Install-Module ExchangeOnlineManagement -Scope CurrentUser -Force -AllowClobber -Confirm:$false
    }
    Import-Module ExchangeOnlineManagement -ErrorAction Stop
    Connect-ExchangeOnline -ShowBanner:$false

    $scopeName = "Metis-$($Mailbox.Split('@')[0])"
    $restricted = $false

    try {
        # Exchange needs its own record of the service principal before it can
        # be given a role assignment.
        if (-not (Get-ServicePrincipal -Identity $app.AppId -ErrorAction SilentlyContinue)) {
            New-ServicePrincipal -AppId $app.AppId -ObjectId $sp.Id -DisplayName $AppName | Out-Null
            Write-Ok "Registered the service principal in Exchange Online"
        }

        if (-not (Get-ManagementScope -Identity $scopeName -ErrorAction SilentlyContinue)) {
            New-ManagementScope -Name $scopeName `
                -RecipientRestrictionFilter "PrimarySmtpAddress -eq '$Mailbox'" | Out-Null
            Write-Ok "Created management scope '$scopeName' (exactly one mailbox)"
        }

        foreach ($r in @("Application Mail.ReadWrite", "Application Mail.Send")) {
            $assignment = "$scopeName-$($r.Replace(' ','-'))"
            if (-not (Get-ManagementRoleAssignment -Identity $assignment -ErrorAction SilentlyContinue)) {
                New-ManagementRoleAssignment -Name $assignment -App $app.AppId `
                    -Role $r -CustomResourceScope $scopeName | Out-Null
            }
            Write-Ok "Granted '$r' scoped to $Mailbox"
        }
        $restricted = $true
        Write-Ok "RBAC for Applications configured - the app reaches $Mailbox and nothing else"
        Write-Host "    (review with: Get-ManagementRoleAssignment -App $($app.AppId) | Format-Table Name,Role,CustomResourceScope)"
    } catch {
        Write-Warn "RBAC for Applications unavailable here: $($_.Exception.Message)"
        Write-Warn "Falling back to the legacy Application Access Policy."
    }

    if (-not $restricted) {
        try {
            New-ApplicationAccessPolicy -AppId $app.AppId `
                -PolicyScopeGroupId $Mailbox -AccessRight RestrictAccess `
                -Description "Metis may access only $Mailbox" | Out-Null
            Write-Ok "Access policy created - the app reaches $Mailbox and nothing else"
            Write-Host "    (verify with: Test-ApplicationAccessPolicy -Identity $Mailbox -AppId $($app.AppId))"
            Write-Warn "This is the legacy mechanism; Microsoft is migrating customers to RBAC for Applications."
        } catch {
            Write-Warn "Could not restrict the app: $($_.Exception.Message)"
            Write-Warn "DO THIS MANUALLY before using the app - it can otherwise read every mailbox:"
            Write-Warn "  New-ApplicationAccessPolicy -AppId $($app.AppId) -PolicyScopeGroupId $Mailbox -AccessRight RestrictAccess -Description 'Metis'"
        }
    }
}

# ── 8. Output / config ────────────────────────────────────────────────────
Write-Step "Done - Metis email settings"
Write-Host ""
Write-Host "  provider            graph"
Write-Host "  graphTenantId       $TenantId"
Write-Host "  graphClientId       $($app.AppId)"
Write-Host "  graphClientSecret   $Secret"
Write-Host "  graphUserId         $Mailbox"
Write-Host ""
Write-Warn "The secret cannot be retrieved again - store it now."

if ($WriteConfig) {
    $cfgPath = Join-Path $HOME ".metis\config.json"
    if (-not (Test-Path $cfgPath)) {
        Write-Warn "No config at $cfgPath - skipping write."
    } else {
        $backup = "$cfgPath.bak-$(Get-Date -Format yyyyMMdd-HHmmss)"
        Copy-Item $cfgPath $backup
        Write-Ok "Backed up config to $backup"

        $cfg = Get-Content $cfgPath -Raw | ConvertFrom-Json
        if (-not $cfg.channels) { $cfg | Add-Member channels ([pscustomobject]@{}) -Force }
        if (-not $cfg.channels.email) { $cfg.channels | Add-Member email ([pscustomobject]@{}) -Force }
        $e = $cfg.channels.email
        foreach ($kv in @{
            provider          = "graph"
            graphTenantId     = $TenantId
            graphClientId     = $app.AppId
            graphClientSecret = $Secret
            graphUserId       = $Mailbox
        }.GetEnumerator()) {
            $e | Add-Member $kv.Key $kv.Value -Force
        }
        $cfg | ConvertTo-Json -Depth 20 | Set-Content $cfgPath -Encoding utf8
        Write-Ok "Wrote Graph settings into $cfgPath"
        Write-Warn "config.json now contains a client secret - keep it out of any repo or backup you share."
    }
}

Write-Host "`nRestart the Metis gateway to pick up the new settings.`n" -ForegroundColor Cyan
