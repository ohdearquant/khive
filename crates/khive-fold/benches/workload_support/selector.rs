use khive_fold::{GreedySelector, Selector, SelectorInput, SelectorOutput, SelectorWeights};

pub struct Fixture {
    pub inputs: Vec<SelectorInput<usize>>,
    pub budget: usize,
    pub weights: SelectorWeights,
    expected_order: Vec<usize>,
}
impl Fixture {
    pub fn precision(count: usize) -> Self {
        let inputs = (0..count)
            .map(|index| SelectorInput {
                id: format!("precision-{index:06}"),
                content: index,
                size: 1,
                score: index as f32 / count as f32,
                category: None,
                information_gain: Some(2.0 * (count - index) as f32),
                rank_score: Some(index as f64 / count as f64),
            })
            .collect();
        let fixture = Self {
            inputs,
            budget: count / 5,
            weights: SelectorWeights {
                epistemic_weight: 0.5,
                ..Default::default()
            },
            expected_order: (0..count / 5).collect(),
        };
        let actual = GreedySelector
            .select(fixture.inputs.clone(), fixture.budget, &fixture.weights)
            .expect("epistemic preflight");
        fixture.validate(&actual);
        let zero = GreedySelector
            .select(
                fixture.inputs.clone(),
                fixture.budget,
                &SelectorWeights::default(),
            )
            .expect("zero epistemic control");
        let zero_order: Vec<_> = zero.selected.iter().map(|item| item.content).collect();
        assert_eq!(
            zero_order,
            (count - fixture.budget..count).rev().collect::<Vec<_>>()
        );
        assert_ne!(zero_order, fixture.expected_order);
        fixture
    }
    pub fn diversity(count: usize) -> Self {
        let mut seed = 0xdead_cafe_u64;
        let inputs = (0..count)
            .map(|index| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                SelectorInput {
                    id: format!("item-{index:06}"),
                    content: index,
                    size: index % 50 + 1,
                    score: (seed >> 32) as f32 / u32::MAX as f32,
                    category: Some(format!("cat-{}", index % 8)),
                    information_gain: None,
                    rank_score: None,
                }
            })
            .collect();
        let mut fixture = Self {
            inputs,
            budget: count * 25,
            weights: SelectorWeights {
                diversity_bias: 0.5,
                ..Default::default()
            },
            expected_order: Vec::new(),
        };
        let actual = GreedySelector
            .select(fixture.inputs.clone(), fixture.budget, &fixture.weights)
            .expect("diversity preflight");
        fixture.expected_order = actual.selected.iter().map(|item| item.content).collect();
        fixture.validate(&actual);
        let zero = GreedySelector
            .select(
                fixture.inputs.clone(),
                fixture.budget,
                &SelectorWeights::default(),
            )
            .expect("zero diversity control");
        assert_ne!(
            fixture.expected_order,
            zero.selected
                .iter()
                .map(|item| item.content)
                .collect::<Vec<_>>()
        );
        let k = actual.selected.len();
        assert!(
            k >= count / 2,
            "fixture must exercise a substantial selected prefix"
        );
        assert_eq!(
            actual
                .selected
                .iter()
                .filter_map(|item| item.category.as_deref())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            8
        );
        let visits = k * (2 * count - k + 1) / 2 + count.saturating_sub(k);
        eprintln!("selector/diversity/{count}: selected={k}, total_size={}, budget={}, reference_loop_remaining_item_visits={visits}, reference_loop_worst_case_visits={}", actual.total_size, actual.budget, count * (count + 1) / 2);
        fixture
    }
    pub fn validate(&self, actual: &SelectorOutput<usize>) {
        assert_eq!(actual.budget, self.budget);
        assert_eq!(
            actual
                .selected
                .iter()
                .map(|item| item.content)
                .collect::<Vec<_>>(),
            self.expected_order
        );
        let mut seen = vec![false; self.inputs.len()];
        let mut size = 0;
        for item in &actual.selected {
            assert!(item.content < seen.len());
            assert!(!seen[item.content]);
            seen[item.content] = true;
            let expected = &self.inputs[item.content];
            assert_eq!(item.id, expected.id);
            assert_eq!(item.size, expected.size);
            assert_eq!(item.score, expected.score);
            assert_eq!(item.category, expected.category);
            assert_eq!(item.information_gain, expected.information_gain);
            assert_eq!(item.rank_score, expected.rank_score);
            size += item.size;
        }
        assert_eq!(actual.total_size, size);
        assert!(size <= self.budget);
        assert!(!actual.selected.is_empty());
    }
}
