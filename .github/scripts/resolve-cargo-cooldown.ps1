param(
    [Parameter(Mandatory = $true)]
    [string] $ManifestPath,

    [ValidateRange(1, 30)]
    [int] $MinimumAgeDays = 3,

    [switch] $Resolve
)

$ErrorActionPreference = "Stop"
$PSNativeCommandUseErrorActionPreference = $false

$resolvedManifestPath = (Resolve-Path -LiteralPath $ManifestPath).Path
$lockPath = Join-Path (Split-Path -Parent $resolvedManifestPath) "Cargo.lock"
$cutoff = [DateTimeOffset]::UtcNow.AddDays(-$MinimumAgeDays)

if (-not (Test-Path -LiteralPath $lockPath)) {
    throw "Cargo.lock was not found next to $resolvedManifestPath"
}

if ($env:CARGO_HOME) {
    $cargoDirectory = $env:CARGO_HOME
} elseif ($env:USERPROFILE) {
    $cargoDirectory = Join-Path $env:USERPROFILE ".cargo"
} else {
    throw "Neither CARGO_HOME nor USERPROFILE is available."
}

$indexRoot = Join-Path $cargoDirectory "registry\index"

function Get-LockedCratesIoPackages {
    $lockContent = Get-Content -LiteralPath $lockPath -Raw
    $blocks = [regex]::Split($lockContent, "(?m)(?=^\[\[package\]\]\s*$)")
    $packages = @()

    foreach ($block in $blocks) {
        $nameMatch = [regex]::Match($block, '(?m)^name = "([^"]+)"\s*$')
        $versionMatch = [regex]::Match($block, '(?m)^version = "([^"]+)"\s*$')
        $sourceMatch = [regex]::Match($block, '(?m)^source = "([^"]+)"\s*$')

        if (-not ($nameMatch.Success -and $versionMatch.Success -and $sourceMatch.Success)) {
            continue
        }

        if ($sourceMatch.Groups[1].Value -notmatch 'crates\.io-index|index\.crates\.io') {
            continue
        }

        $packages += [pscustomobject]@{
            Name = $nameMatch.Groups[1].Value
            Version = $versionMatch.Groups[1].Value
        }
    }

    if ($packages.Count -eq 0) {
        throw (
            "No crates.io packages were found in $lockPath. " +
            "The lockfile format may have changed; cooldown was NOT verified."
        )
    }

    return $packages
}

function Get-SparseCachePath {
    param([Parameter(Mandatory = $true)][string] $CrateName)

    $lowerName = $CrateName.ToLowerInvariant()
    switch ($lowerName.Length) {
        1 { $relativePath = Join-Path "1" $lowerName }
        2 { $relativePath = Join-Path "2" $lowerName }
        3 { $relativePath = Join-Path (Join-Path "3" $lowerName.Substring(0, 1)) $lowerName }
        default {
            $relativePath = Join-Path (
                Join-Path $lowerName.Substring(0, 2) $lowerName.Substring(2, 2)
            ) $lowerName
        }
    }

    $cacheMatches = @(
        Get-ChildItem -LiteralPath $indexRoot -Directory -ErrorAction SilentlyContinue |
            ForEach-Object {
                $candidate = Join-Path (Join-Path $_.FullName ".cache") $relativePath
                if (Test-Path -LiteralPath $candidate) {
                    $candidate
                }
            }
    )

    if ($cacheMatches.Count -eq 0) {
        throw "Sparse-index cache entry not found for crate '$CrateName'."
    }

    return $cacheMatches[0]
}

function Get-SparseVersions {
    param([Parameter(Mandatory = $true)][string] $CrateName)

    $cachePath = Get-SparseCachePath -CrateName $CrateName
    $cacheText = [Text.Encoding]::UTF8.GetString([IO.File]::ReadAllBytes($cachePath))
    $versions = @()

    foreach ($segment in $cacheText.Split([char]0)) {
        $trimmed = $segment.Trim()
        if (-not $trimmed.StartsWith('{"name"')) {
            continue
        }

        try {
            $record = $trimmed | ConvertFrom-Json
        } catch {
            continue
        }

        if ($record.name -eq $CrateName -and $record.pubtime) {
            $versions += $record
        }
    }

    if ($versions.Count -eq 0) {
        throw "No publication timestamps were found in the sparse-index cache for '$CrateName'."
    }

    return $versions
}

function Invoke-Cargo {
    param([Parameter(Mandatory = $true)][string[]] $Arguments)

    & cargo @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo $($Arguments -join ' ') failed with exit code $LASTEXITCODE."
    }
}

$maximumResolutionAttempts = 100
for ($attempt = 1; $attempt -le $maximumResolutionAttempts; $attempt++) {
    $recentPackage = $null
    $recentPackageVersions = $null

    foreach ($package in (Get-LockedCratesIoPackages)) {
        $availableVersions = @(Get-SparseVersions -CrateName $package.Name)
        $lockedRecord = $availableVersions |
            Where-Object { $_.vers -eq $package.Version } |
            Select-Object -First 1

        if (-not $lockedRecord) {
            throw "Publication timestamp not found for $($package.Name)@$($package.Version)."
        }

        $publicationTime = [DateTimeOffset]::Parse(
            $lockedRecord.pubtime,
            [Globalization.CultureInfo]::InvariantCulture
        )
        $age = [DateTimeOffset]::UtcNow - $publicationTime
        Write-Host (
            "Checked {0}@{1}: published {2:u}, age {3:N2} days" -f
            $package.Name,
            $package.Version,
            $publicationTime,
            $age.TotalDays
        )

        if ($publicationTime -gt $cutoff) {
            $recentPackage = $package
            $recentPackageVersions = $availableVersions
            break
        }
    }

    if (-not $recentPackage) {
        Write-Host "All locked crates.io packages have been public for at least $MinimumAgeDays full days."
        exit 0
    }

    if (-not $Resolve) {
        throw (
            (
                "{0}@{1} has not been public for {2} full days. " +
                "Run this script locally with -Resolve, review and commit Cargo.lock, then retry."
            ) -f
            $recentPackage.Name,
            $recentPackage.Version,
            $MinimumAgeDays
        )
    }

    Write-Warning (
        "{0}@{1} is newer than the cutoff {2:u}; searching for the newest compatible older version." -f
        $recentPackage.Name,
        $recentPackage.Version,
        $cutoff
    )

    $currentSemanticVersion = [System.Management.Automation.SemanticVersion]::new(
        $recentPackage.Version
    )
    $candidates = @(
        $recentPackageVersions |
            Where-Object {
                -not $_.yanked -and
                ([DateTimeOffset]::Parse(
                    $_.pubtime,
                    [Globalization.CultureInfo]::InvariantCulture
                ) -le $cutoff) -and
                ([System.Management.Automation.SemanticVersion]::new($_.vers) -lt $currentSemanticVersion)
            } |
            ForEach-Object {
                [pscustomobject]@{
                    Version = $_.vers
                    SemanticVersion = [System.Management.Automation.SemanticVersion]::new($_.vers)
                }
            } |
            Sort-Object SemanticVersion -Descending
    )

    $resolved = $false
    foreach ($candidate in $candidates) {
        Write-Host (
            "Trying cargo update -p {0}@{1} --precise {2}" -f
            $recentPackage.Name,
            $recentPackage.Version,
            $candidate.Version
        )

        & cargo update `
            --manifest-path $resolvedManifestPath `
            -p "$($recentPackage.Name)@$($recentPackage.Version)" `
            --precise $candidate.Version

        if ($LASTEXITCODE -eq 0) {
            Write-Host (
                "Downgraded {0}@{1} to the newest compatible eligible version, {2}." -f
                $recentPackage.Name,
                $recentPackage.Version,
                $candidate.Version
            )
            Invoke-Cargo -Arguments @(
                "fetch",
                "--locked",
                "--manifest-path",
                $resolvedManifestPath
            )
            $resolved = $true
            break
        }
    }

    if (-not $resolved) {
        throw (
            "No compatible version of {0} older than {1:u} could be resolved. Build stopped." -f
            $recentPackage.Name,
            $cutoff
        )
    }
}

throw "Dependency cooldown resolution exceeded $maximumResolutionAttempts attempts."
