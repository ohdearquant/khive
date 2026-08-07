import { Studio } from "@/components/studio";
import { fixtureReviewSource } from "@/lib/adapters/fixture-review-source";

export default async function Home() {
  const bundle = await fixtureReviewSource.load();

  return <Studio initialBundle={bundle} />;
}
