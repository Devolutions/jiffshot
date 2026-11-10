function Get-MsixPublisher {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [object]$Signature,
        [Parameter(Mandatory)]
        [bool]$RequireSigned
    )

    if ($Signature.Status -eq 'Valid') {
        if ($null -eq $Signature.SignerCertificate -or
            [string]::IsNullOrWhiteSpace($Signature.SignerCertificate.Subject)) {
            throw 'The executable signature is valid but has no signer subject.'
        }
        return $Signature.SignerCertificate.Subject
    }

    if (-not $RequireSigned -and $Signature.Status -eq 'NotSigned') {
        return 'CN=Devolutions Jiffshot CI'
    }
    throw "Cannot determine the MSIX publisher from the executable signature ($($Signature.Status)): $($Signature.StatusMessage)"
}
