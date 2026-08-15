# Terms of Service

> **TEMPLATE — NOT DEPLOYED OR LEGALLY APPROVED.** Replace every `[[REQUIRED: ...]]` item, align these terms with the actual distribution/payment/support model, obtain counsel approval, and publish on the official HTTPS domain before use.

**Effective date:** [[REQUIRED: YYYY-MM-DD]]

**Provider:** [[REQUIRED: LEGAL ENTITY, ADDRESS, COUNTRY]]

## 1. Agreement

These Terms govern your use of the rec_bag desktop application, hosted SoundCloud OAuth broker, website, and related support (“Service”). By using the Service, you agree to these Terms and the [Privacy Policy](privacy-policy.md). If you do not agree, do not use the Service.

[[REQUIRED: LEGAL REVIEW — contracting entity, acceptance mechanism, consumer notices, age/capacity, and regional terms.]]

## 2. Service description

rec_bag helps you catalog music metadata, connect a read-only SoundCloud catalog, associate music files you lawfully control, manage one local or Google Drive primary, create local/Drive backups, verify files, schedule synchronization, and recover catalog snapshots.

Google Drive integration requests `drive.file` and creates a dedicated My Drive folder named `rec_bag Music`. SoundCloud integration reads connected-account uploads, created playlists, likes, liked playlists, and their metadata/entries. Provider services remain governed by Google’s and SoundCloud’s terms and availability.

## 3. Your accounts and credentials

You are responsible for:

- maintaining the security of your OS account, provider accounts, devices, and recovery materials;
- granting only accounts you are authorized to use;
- reviewing OAuth consent and revoking access when no longer needed;
- keeping one active rec_bag manager per profile and using confirmed takeover/recovery procedures;
- promptly reporting suspected unauthorized access to **[[REQUIRED: SECURITY CONTACT]]**.

Provider credentials are stored in the OS credential store. Copying a catalog does not copy credentials.

## 4. Your content and rights

You retain rights in your music files, playlists, metadata, and other content. You grant **[[REQUIRED: LEGAL ENTITY]]** only the limited rights necessary to process data and operate requested features, including transmitting data to Google or SoundCloud and processing token exchanges through the broker.

You represent that you have all rights and permissions required to copy, upload, back up, analyze, play, and manage the content you use with rec_bag. rec_bag does not grant rights to download, archive, or redistribute third-party music.

## 5. SoundCloud restrictions

SoundCloud is used as a catalog source. You must comply with SoundCloud’s terms and developer policies. You may not use rec_bag to:

- archive SoundCloud playback transcodes or circumvent download/access restrictions;
- misrepresent ownership or authorization;
- scrape, resell, redistribute, or create unauthorized copies of SoundCloud content;
- modify imported SoundCloud playlists as though rec_bag controlled the provider copy.

Only files you lawfully own or are authorized to store may become primary/backup assets.

## 6. Google Drive restrictions

You must comply with Google’s terms and policies. rec_bag uses the app-created `rec_bag Music` folder and does not request broad full-Drive access. Do not use rec_bag to upload unlawful content, exceed quotas abusively, bypass access controls, or interfere with Google services.

Disconnecting, revoking, clearing cache, or forgetting local provider data does not delete files from Google Drive. Forgetting is destructive to current local provider-derived source/storage rows and current catalog snapshot records, but preserves canonical owned assets and credentials unless separately removed. You control remote deletion in Drive.

## 7. Backups and recovery

Backups reduce risk but are not guaranteed. You are responsible for maintaining independent, tested backups and sufficient storage/provider availability. Primary deletion does not automatically delete backups. Default version retention is 30 days and at least three recent versions, subject to configured storage capabilities and explicit cleanup approval.

Recovery uses integrity checks, dry-run validation, pre-restore snapshots, exact transactional replacement, exact rollback, and ownership safeguards. Rows absent from the selected snapshot are deleted from snapshot-owned tables. Recovery does not reconstruct provider credentials or missing remote/local files. Verify restored assets before relying on them. Copying a catalog to another device does not transfer authority; portable authority requires an explicit confirmed handoff.

## 8. Acceptable use

You may not:

- violate law, intellectual-property rights, privacy rights, provider policies, or sanctions/export controls;
- access accounts, devices, or files without authorization;
- attack, overload, probe, reverse engineer contrary to applicable law/license, or bypass security/rate limits;
- submit malware, secrets in support tickets, or content intended to compromise the Service;
- use the broker as a general token proxy or expose its endpoints outside approved clients.

[[REQUIRED: LEGAL REVIEW — open-source license interaction, reverse-engineering exceptions, sanctions, and enforcement process.]]

## 9. Privacy

Our data practices are described in the [Privacy Policy](privacy-policy.md). Deletion and revocation procedures are in [Data Deletion](data-deletion.md). Support channels are in [Support](support.md).

## 10. Availability and changes

The Service depends on your device, filesystem, OS credential service, network, SoundCloud, Google Drive, and hosted infrastructure. Features may be interrupted by quotas, policy changes, provider outages, revocation, storage failure, or updates.

[[REQUIRED: DEFINE service levels, maintenance notices, deprecation policy, and whether hosted services may be suspended or discontinued.]]

## 11. Fees

[[REQUIRED: STATE free/paid model, taxes, billing, renewals, cancellation, refunds, and consumer withdrawal rights; or explicitly state no fees after legal review.]]

Provider, network, and storage charges are your responsibility.

## 12. Termination

You may stop using rec_bag, disconnect/revoke providers, and delete local/remote data using [Data Deletion](data-deletion.md). We may restrict hosted-service access for security, legal, policy, or material Terms violations under **[[REQUIRED: NOTICE/APPEAL PROCESS]]**.

Termination does not itself delete local files, backups, recovery snapshots, or Google Drive files. Follow each deletion step explicitly.

## 13. Disclaimers

THE SERVICE IS PROVIDED **[[REQUIRED: WARRANTY LANGUAGE AND MANDATORY CONSUMER-LAW CARVE-OUTS]]**. Audio analysis, matching, synchronization, backup, and recovery results may be incomplete or incorrect. Verify important files and metadata independently.

## 14. Limitation of liability

[[REQUIRED: COUNSEL-DRAFTED LIABILITY CAP, EXCLUDED DAMAGES, DATA-LOSS TERMS, AND JURISDICTION-SPECIFIC CARVE-OUTS.]]

## 15. Indemnity

[[REQUIRED: COUNSEL REVIEW — whether indemnity is appropriate, scope, defense control, and consumer exceptions.]]

## 16. Governing law and disputes

[[REQUIRED: GOVERNING LAW, VENUE, INFORMAL RESOLUTION, ARBITRATION/CLASS WAIVER IF APPLICABLE, OPT-OUT, AND CONSUMER RIGHTS.]]

## 17. Changes to these Terms

We will publish changes at **[[REQUIRED: FINAL TERMS URL]]** and update the effective date. [[REQUIRED: MATERIAL-CHANGE NOTICE AND ACCEPTANCE PROCESS.]]

## 18. Contact

**[[REQUIRED: LEGAL ENTITY]]**

**[[REQUIRED: POSTAL ADDRESS]]**

**[[REQUIRED: LEGAL/SUPPORT EMAIL]]**

[Support](support.md)
