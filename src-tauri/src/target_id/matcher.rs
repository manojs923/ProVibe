use super::scanner::ScannedArtifact;
use super::{profiles, TargetProfile};
use std::collections::HashMap;

const MATCH_THRESHOLD: u32 = 50;

/// scores the scanned host artifacts against known profiles to determine the target.
/// uses a weighted fuzzy matching approach to survive version drift.
pub fn match_artifacts(artifacts: Vec<ScannedArtifact>) -> Option<TargetProfile> {
    let profiles = profiles::fetch_all();
    let mut scores: HashMap<String, u32> = HashMap::new();

    // in a real scenario, you'd also pass in registry and service artifacts.
    // since the scanner currently only grabs processes, we score based on that.
    
    for artifact in &artifacts {
        let exe_lower = artifact.executable_name.to_lowercase();
        
        for profile in &profiles {
            let profile_name = profile.display_name.clone();
            
            for target_proc in &profile.detection_vectors.process_names {
                let target_lower = target_proc.to_lowercase();
                
                // exact match on executable name
                if exe_lower == target_lower {
                    *scores.entry(profile_name.clone()).or_insert(0) += 40;
                    
                    // if it matches the name AND has a valid digital signature,
                    // it's almost certainly the real deal, not a false positive.
                    if artifact.is_signed {
                        *scores.entry(profile_name.clone()).or_insert(0) += 30;
                    }
                }
                
                // partial match (e.g., "respondus" in "respondus lockdown browser.exe")
                // less weight, but helps catch slight renames
                else if exe_lower.contains(&target_lower) || target_lower.contains(&exe_lower) {
                    *scores.entry(profile_name.clone()).or_insert(0) += 15;
                }
            }
        }
    }

    // find the profile with the highest score
    let best_match = scores.into_iter().max_by_key(|&(_, score)| score);

    if let Some((best_profile_name, score)) = best_match {
        if score >= MATCH_THRESHOLD {
            // return the full profile struct that matches the winning name
            return profiles.into_iter().find(|p| p.display_name == best_profile_name);
        }
    }

    None
}