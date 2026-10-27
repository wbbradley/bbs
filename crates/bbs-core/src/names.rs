//! Seed pool of agent names: popular given names from major languages written
//! in the Latin alphabet, restricted to plain ASCII so they are easy to type.

#[rustfmt::skip]
pub const NAMES: [&str; 256] = [
    // English
    "James", "Mary", "John", "Patricia", "Robert", "Jennifer", "Michael", "Linda",
    "William", "Elizabeth", "David", "Barbara", "Richard", "Susan", "Joseph", "Jessica",
    "Thomas", "Sarah", "Charles", "Karen", "Daniel", "Nancy", "Matthew", "Lisa",
    "Anthony", "Emily", "Mark", "Emma", "Olivia", "Liam", "Noah", "Ava",
    "Sophia", "Isabella", "Mia", "Amelia", "Harper", "Ethan", "Mason", "Logan",
    "Lucas", "Oliver", "Charlotte", "Grace", "Chloe", "Ella", "Henry", "Jack",
    "Samuel", "Benjamin",
    // Spanish
    "Santiago", "Mateo", "Sebastian", "Diego", "Alejandro", "Valentina", "Camila", "Lucia",
    "Sofia", "Martina", "Javier", "Carlos", "Miguel", "Pablo", "Carmen", "Isabel",
    "Elena", "Paula", "Alvaro", "Hugo", "Adrian", "Ximena", "Fernanda", "Gabriela",
    "Andres", "Rodrigo", "Catalina", "Renata", "Emilio", "Joaquin",
    // Portuguese
    "Joao", "Pedro", "Gabriel", "Rafael", "Gustavo", "Felipe", "Bruno", "Thiago",
    "Leonardo", "Beatriz", "Mariana", "Larissa", "Juliana", "Leticia", "Ana", "Clara",
    "Heloisa", "Bernardo", "Davi", "Enzo",
    // French
    "Louis", "Jules", "Arthur", "Leo", "Raphael", "Adam", "Theo", "Maxime",
    "Antoine", "Nicolas", "Camille", "Manon", "Ines", "Jade", "Louise", "Lea",
    "Zoe", "Margaux", "Juliette", "Amelie", "Pierre", "Julien", "Mathis",
    // German
    "Lukas", "Felix", "Jonas", "Leon", "Paul", "Maximilian", "Finn", "Elias",
    "Ben", "Hannah", "Lena", "Leonie", "Anna", "Marie", "Johanna", "Greta",
    "Klaus", "Jurgen", "Sabine", "Stefan", "Wolfgang", "Ingrid", "Heike", "Matthias",
    // Italian
    "Francesco", "Alessandro", "Lorenzo", "Giuseppe", "Marco", "Giovanni", "Luca", "Andrea",
    "Giulia", "Chiara", "Francesca", "Alessia", "Aurora", "Ginevra", "Federica", "Simone",
    "Davide", "Riccardo", "Vittoria",
    // Dutch
    "Daan", "Sem", "Bram", "Lotte", "Sanne", "Femke", "Joris", "Pieter",
    "Anouk", "Ruben",
    // Scandinavian
    "Erik", "Lars", "Nils", "Astrid", "Freya", "Sigrid", "Bjorn", "Elsa",
    "Saga", "Oskar",
    // Polish / Czech
    "Jakub", "Kacper", "Zofia", "Wojciech", "Agnieszka", "Katarzyna", "Tomasz", "Magda",
    "Ondrej", "Tereza",
    // Turkish
    "Mehmet", "Mustafa", "Ahmet", "Emre", "Elif", "Zeynep", "Ayse", "Fatma",
    "Kerem", "Defne",
    // Indonesian / Malay
    "Budi", "Siti", "Dewi", "Agus", "Putri", "Rizky", "Wahyu", "Ayu",
    // Vietnamese
    "Minh", "Linh", "Huong", "Tuan", "Thanh", "Mai",
    // Swahili / Igbo / Akan / Sotho
    "Amani", "Baraka", "Zawadi", "Juma", "Neema", "Chidi", "Ngozi", "Kwame",
    "Ama", "Thabo",
    // Tagalog
    "Jose", "Maria", "Angelo", "Rosa",
    // Yoruba
    "Ade", "Tunde", "Folake", "Bisi",
    // Hausa
    "Musa", "Aisha", "Ibrahim", "Fatima",
    // Hungarian
    "Bence", "Levente", "Eszter", "Zsofia",
    // Romanian
    "Andrei", "Ioana", "Mihai", "Bogdan",
    // Irish
    "Siobhan", "Niamh", "Sean", "Aoife", "Cian", "Oisin",
];

#[cfg(test)]
mod tests {
    use super::NAMES;
    use std::collections::HashSet;

    #[test]
    fn names_are_unique_ascii_and_capitalized() {
        let mut seen = HashSet::new();
        for name in NAMES {
            assert!(name.chars().all(|c| c.is_ascii_alphabetic()), "{name}");
            assert!(name.starts_with(|c: char| c.is_ascii_uppercase()), "{name}");
            assert!(seen.insert(name.to_lowercase()), "duplicate name {name}");
        }
    }
}
