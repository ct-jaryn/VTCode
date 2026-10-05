function isRecord(value: unknown): value is Record<string, unknown> {
    return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Check a contribution's literal identifier without trusting manifest shapes. */
export function hasManifestContribution(
    manifest: unknown,
    group: "languageModelTools" | "chatParticipants",
    identifier: string
): boolean {
    if (!isRecord(manifest) || !isRecord(manifest.contributes)) {
        return false;
    }
    const contributions = manifest.contributes[group];
    const key = group === "languageModelTools" ? "name" : "id";
    return Array.isArray(contributions) && contributions.some((entry: unknown) =>
        isRecord(entry) && entry[key] === identifier
    );
}
