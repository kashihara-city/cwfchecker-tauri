param(
    [Parameter(Mandatory = $true)]
    [string] $ManifestPath,

    [ValidateRange(1, 30)]
    [int] $MinimumAgeDays = 14,

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

function ConvertTo-PublicationTime {
    param([Parameter(Mandatory = $true)] $Value)

    # PowerShellはISO 8601文字列をConvertFrom-Json時にDateTimeへ変換する。
    # それを文字列として再Parseすると実行環境のタイムゾーンが混入するため、
    # DateTimeのKindを保ったままDateTimeOffsetへ変換する。
    if ($Value -is [DateTimeOffset]) {
        return $Value
    }
    if ($Value -is [DateTime]) {
        return [DateTimeOffset]::new($Value.ToUniversalTime())
    }
    return [DateTimeOffset]::Parse(
        $Value,
        [Globalization.CultureInfo]::InvariantCulture,
        [Globalization.DateTimeStyles]::AssumeUniversal
    )
}

function Test-CompatibleVersionBand {
    param(
        [Parameter(Mandatory = $true)]
        [System.Management.Automation.SemanticVersion] $Current,

        [Parameter(Mandatory = $true)]
        [System.Management.Automation.SemanticVersion] $Candidate
    )

    if ($Current.Major -gt 0) {
        return $Candidate.Major -eq $Current.Major
    }
    if ($Current.Minor -gt 0) {
        return $Candidate.Major -eq 0 -and $Candidate.Minor -eq $Current.Minor
    }
    return $Candidate.Major -eq 0 -and
        $Candidate.Minor -eq 0 -and
        $Candidate.Patch -eq $Current.Patch
}

function Get-RecentPackages {
    $recentPackages = @()
    $checkedCount = 0

    foreach ($package in (Get-LockedCratesIoPackages)) {
        $checkedCount++
        $availableVersions = @(Get-SparseVersions -CrateName $package.Name)
        $lockedRecord = $availableVersions |
            Where-Object { $_.vers -eq $package.Version } |
            Select-Object -First 1

        if (-not $lockedRecord) {
            throw "Publication timestamp not found for $($package.Name)@$($package.Version)."
        }

        $publicationTime = ConvertTo-PublicationTime -Value $lockedRecord.pubtime
        if ($publicationTime -le $cutoff) {
            continue
        }

        $age = [DateTimeOffset]::UtcNow - $publicationTime
        Write-Host (
            "Cooldown pending: {0}@{1}, published {2:u}, age {3:N2} days" -f
            $package.Name,
            $package.Version,
            $publicationTime,
            $age.TotalDays
        )
        $recentPackages += [pscustomobject]@{
            Package = $package
            AvailableVersions = $availableVersions
        }
    }

    Write-Host "Checked $checkedCount locked crates.io package entries."
    return $recentPackages
}

function Get-EligibleCandidates {
    param([Parameter(Mandatory = $true)] $RecentPackage)

    $currentVersion = [System.Management.Automation.SemanticVersion]::new(
        $RecentPackage.Package.Version
    )
    return @(
        $RecentPackage.AvailableVersions |
            Where-Object {
                $candidateVersion = [System.Management.Automation.SemanticVersion]::new($_.vers)
                -not $_.yanked -and
                ((ConvertTo-PublicationTime -Value $_.pubtime) -le $cutoff) -and
                ($candidateVersion -lt $currentVersion) -and
                (Test-CompatibleVersionBand `
                    -Current $currentVersion `
                    -Candidate $candidateVersion)
            } |
            ForEach-Object {
                [pscustomobject]@{
                    Version = $_.vers
                    SemanticVersion = [System.Management.Automation.SemanticVersion]::new($_.vers)
                }
            } |
            Sort-Object SemanticVersion -Descending
    )
}

$maximumResolutionAttempts = 100
for ($attempt = 1; $attempt -le $maximumResolutionAttempts; $attempt++) {
    $recentPackages = @(Get-RecentPackages)
    if ($recentPackages.Count -eq 0) {
        Write-Host "All locked crates.io packages have been public for at least $MinimumAgeDays full days."
        exit 0
    }

    if (-not $Resolve) {
        $first = $recentPackages[0].Package
        throw (
            (
                "{0} locked package entries have not been public for {1} full days; " +
                "the first is {2}@{3}. Run this script locally with -Resolve, " +
                "review and commit Cargo.lock, then retry."
            ) -f
            $recentPackages.Count,
            $MinimumAgeDays,
            $first.Name,
            $first.Version
        )
    }

    $candidateLists = @($recentPackages | ForEach-Object {
        [pscustomobject]@{
            RecentPackage = $_
            Candidates = @(Get-EligibleCandidates -RecentPackage $_)
        }
    })
    $maximumCandidateCount = ($candidateLists |
        ForEach-Object { $_.Candidates.Count } |
        Measure-Object -Maximum).Maximum
    if (-not $maximumCandidateCount) {
        throw "No cooldown-eligible versions are available for the recent locked packages."
    }

    $resolved = $false
    for ($candidateIndex = 0; $candidateIndex -lt $maximumCandidateCount; $candidateIndex++) {
        foreach ($candidateList in $candidateLists) {
            if ($candidateIndex -ge $candidateList.Candidates.Count) {
                continue
            }

            $recentPackage = $candidateList.RecentPackage.Package
            $candidate = $candidateList.Candidates[$candidateIndex]
            $lockSnapshot = [IO.File]::ReadAllBytes($lockPath)
            Write-Host (
                "Trying cargo update -p {0}@{1} --precise {2}" -f
                $recentPackage.Name,
                $recentPackage.Version,
                $candidate.Version
            )
            $cargoOutput = @(
                & cargo update `
                    --manifest-path $resolvedManifestPath `
                    -p "$($recentPackage.Name)@$($recentPackage.Version)" `
                    --precise $candidate.Version 2>&1
            )

            if ($LASTEXITCODE -ne 0) {
                [IO.File]::WriteAllBytes($lockPath, $lockSnapshot)
                Write-Host (
                    "Deferred {0}@{1}; another recent dependency may need to move first." -f
                    $recentPackage.Name,
                    $recentPackage.Version
                )
                continue
            }

            $cargoOutput | Write-Host
            Write-Host (
                "Resolved {0}@{1} to cooldown-eligible version {2}." -f
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
        if ($resolved) {
            break
        }
    }

    if (-not $resolved) {
        $blocked = $recentPackages |
            ForEach-Object { "$($_.Package.Name)@$($_.Package.Version)" }
        throw (
            "No cooldown-eligible dependency update could be resolved. " +
            "Blocked packages: $($blocked -join ', ')"
        )
    }
}

throw "Dependency cooldown resolution exceeded $maximumResolutionAttempts successful updates."
